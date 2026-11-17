use super::*;
use crate::source_guard::{find, function, has, lost, tokens, update, words};
use crate::test_support::TempDir;

const SIGN_IN: &str = "1:134391268890553375";
const EARLIER: &str = "1:134380000000000000";
const HOUR: Duration = Duration::from_secs(60 * 60);
const EXE: &str = r"C:\Users\synthetic-user\AppData\Local\SlopBar\slopbar.exe";
const SID: &str = "S-1-5-21-1000-1000-1000-1001";

/// A marker's age, in seconds, for each state the decision tells apart: none, fresh, stale.
fn ages(grace: Duration) -> [Option<i64>; 3] {
    [None, Some(grace.as_secs() as i64 - 1), Some(grace.as_secs() as i64)]
}

/// SlopBar stopped in a sign-in it ran in, an hour after it began, with nothing holding the check.
fn stopped() -> Facts {
    Facts {
        running: false,
        enabled: true,
        sign_in_age: Some(HOUR),
        quit: Mark::Absent,
        started: Mark::ThisSignIn,
        launch_at_login: true,
        installer: false,
        installing: None,
        agent_quit: None,
    }
}

// ------------------------------------------------------------------------------------------------
// The decision.

/// Every combination the owner named: SlopBar running or not; a person's Quit from this sign-in,
/// from an earlier one, or none; an install marker fresh, stale or none; quit_app's marker fresh,
/// stale or none. SlopBar is started exactly when it is not running, no person quit it in this
/// sign-in, and neither marker is fresh; otherwise the check leaves it for the first reason that holds.
#[test]
fn every_combination_the_owner_named_is_decided() {
    let mut cases = 0;
    for running in [false, true] {
        for quit in [Mark::Absent, Mark::ThisSignIn, Mark::EarlierSignIn] {
            for (i, installing) in ages(INSTALL_GRACE).into_iter().enumerate() {
                for (a, agent_quit) in ages(AGENT_QUIT_GRACE).into_iter().enumerate() {
                    let facts = Facts { running, quit, installing, agent_quit, ..stopped() };
                    let (install_fresh, agent_fresh) = (i == 1, a == 1);
                    let want = if running {
                        Verdict::Running
                    } else if quit == Mark::ThisSignIn {
                        Verdict::QuitByPerson
                    } else if install_fresh {
                        Verdict::Installing
                    } else if agent_fresh {
                        Verdict::QuitByAgent
                    } else {
                        Verdict::Start
                    };
                    let got = decide(&facts);
                    assert_eq!(got, want, "{facts:?}");
                    let starts = !running && quit != Mark::ThisSignIn && !install_fresh && !agent_fresh;
                    assert_eq!(got == Verdict::Start, starts, "{facts:?}");
                    cases += 1;
                }
            }
        }
    }
    assert_eq!(cases, 2 * 3 * 3 * 3);
}

/// A running SlopBar ends the check first, whatever else holds: nothing else is read then.
#[test]
fn a_running_slopbar_ends_the_check_at_once() {
    let held = [
        Facts { enabled: false, ..stopped() },
        Facts { sign_in_age: None, ..stopped() },
        Facts { quit: Mark::ThisSignIn, installer: true, installing: Some(0), agent_quit: Some(0), ..stopped() },
        Facts { started: Mark::Absent, launch_at_login: false, ..stopped() },
        Facts::default(),
    ];
    for facts in held {
        assert_eq!(decide(&Facts { running: true, ..facts.clone() }), Verdict::Running, "{facts:?}");
    }
}

/// The setting off, or a sign-in that cannot be told (so neither a person's Quit nor SlopBar's last
/// start can be read against it), start nothing.
#[test]
fn the_setting_and_an_unknown_sign_in_hold_the_check() {
    assert_eq!(decide(&Facts { enabled: false, ..stopped() }), Verdict::Off);
    assert_eq!(decide(&Facts { sign_in_age: None, ..stopped() }), Verdict::UnknownSignIn);
    assert_eq!(decide(&Facts { enabled: false, sign_in_age: None, ..stopped() }), Verdict::Off);
    assert_eq!(decide(&stopped()), Verdict::Start);
}

/// An installer that runs holds the check, whatever the install marker says (one started by hand
/// leaves none).
#[test]
fn a_running_installer_holds_the_check() {
    for installing in ages(INSTALL_GRACE) {
        assert_eq!(decide(&Facts { installer: true, installing, ..stopped() }), Verdict::Installing, "{installing:?}");
    }
    assert_eq!(decide(&Facts { installer: true, quit: Mark::ThisSignIn, ..stopped() }), Verdict::QuitByPerson);
}

/// A sign-in SlopBar has not started in: nothing stopped, so the check starts it only where it
/// launches at login, and only once launch at login has had its time; a sign-in it ran in is
/// restarted at once.
#[test]
fn a_sign_in_slopbar_never_ran_in() {
    let young = SIGN_IN_GRACE - Duration::from_secs(1);
    for started in [Mark::Absent, Mark::EarlierSignIn, Mark::ThisSignIn] {
        for launch_at_login in [false, true] {
            for age in [Duration::ZERO, young, SIGN_IN_GRACE, HOUR] {
                let facts = Facts { started, launch_at_login, sign_in_age: Some(age), ..stopped() };
                let want = if started == Mark::ThisSignIn {
                    Verdict::Start
                } else if !launch_at_login {
                    Verdict::NeverStarted
                } else if age < SIGN_IN_GRACE {
                    Verdict::SignInStarting
                } else {
                    Verdict::Start
                };
                assert_eq!(decide(&facts), want, "{facts:?}");
            }
        }
    }
}

/// A marker holds from when it was written until its grace has passed, and also a little ahead of
/// the clock; further ahead it holds nothing, so a clock set back never holds the check for long.
#[test]
fn markers_hold_for_their_grace_and_no_longer() {
    let grace = Duration::from_secs(600);
    assert!(fresh(0, grace));
    assert!(fresh(599, grace));
    assert!(!fresh(600, grace), "the grace has passed");
    assert!(!fresh(86_400, grace));
    assert!(fresh(-60, grace), "a minute ahead of the clock");
    assert!(!fresh(-61, grace), "further ahead");
}

/// A person's Quit and SlopBar's last start, read against this sign-in. A Quit that cannot be read
/// holds (it is never overridden on a guess); a start that cannot be read is no start here.
#[test]
fn markers_are_read_against_this_sign_in() {
    assert_eq!(quit_mark(None, SIGN_IN), Mark::Absent);
    assert_eq!(quit_mark(Some(SIGN_IN), SIGN_IN), Mark::ThisSignIn);
    assert_eq!(quit_mark(Some(&format!("{SIGN_IN}\r\n")), SIGN_IN), Mark::ThisSignIn);
    assert_eq!(quit_mark(Some(EARLIER), SIGN_IN), Mark::EarlierSignIn);
    assert_eq!(quit_mark(Some("2:134391268890553375"), SIGN_IN), Mark::EarlierSignIn, "another session");
    for unreadable in ["", "garbled", "1:", ":5", "1:0", "x:5"] {
        assert_eq!(quit_mark(Some(unreadable), SIGN_IN), Mark::ThisSignIn, "{unreadable:?}");
    }
    assert_eq!(started_mark(None, SIGN_IN), Mark::Absent);
    assert_eq!(started_mark(Some(SIGN_IN), SIGN_IN), Mark::ThisSignIn);
    assert_eq!(started_mark(Some(EARLIER), SIGN_IN), Mark::EarlierSignIn);
    assert_eq!(started_mark(Some(""), SIGN_IN), Mark::EarlierSignIn);
    assert_eq!(sign_in_id(1, 134391268890553375), SIGN_IN);
}

// ------------------------------------------------------------------------------------------------
// The markers on disk.

fn markers() -> (TempDir, Markers) {
    let dir = TempDir::new();
    let markers = Markers(dir.0.join("keep-running"));
    (dir, markers)
}

fn facts(markers: &Markers, sign_in: &str) -> Facts {
    let sign_in = SignIn { id: sign_in.into(), age: HOUR };
    markers.facts(SystemTime::now(), Some(&sign_in), &Settings::default(), false)
}

/// A person's Quit holds for the rest of its sign-in and lapses at the next; a start that is not the
/// watchdog's ends it, the watchdog's own does not. Every start ends the install and quit_app markers.
#[test]
fn starts_and_quits_leave_what_the_check_reads() {
    let (_dir, m) = markers();
    assert_eq!(decide(&facts(&m, SIGN_IN)), Verdict::NeverStarted, "nothing yet, and no launch at login (the default)");

    m.record_start(Some(SIGN_IN), false);
    assert_eq!(facts(&m, SIGN_IN).started, Mark::ThisSignIn);
    assert_eq!(decide(&facts(&m, SIGN_IN)), Verdict::Start, "it ran here and stopped");

    m.record_quit(Some(SIGN_IN));
    assert_eq!(decide(&facts(&m, SIGN_IN)), Verdict::QuitByPerson);
    assert_eq!(facts(&m, "1:134399999999999999").quit, Mark::EarlierSignIn, "the next sign-in");

    m.record_start(Some(SIGN_IN), true);
    assert_eq!(decide(&facts(&m, SIGN_IN)), Verdict::QuitByPerson, "the watchdog's own start ends no Quit");
    m.record_start(Some(SIGN_IN), false);
    assert_eq!(facts(&m, SIGN_IN).quit, Mark::Absent, "any other start does");

    m.record_quit(None);
    assert_eq!(decide(&facts(&m, SIGN_IN)), Verdict::QuitByPerson, "a Quit naming no sign-in holds");

    m.record_start(Some(SIGN_IN), false);
    m.record_install("0.1.0-ultra.99");
    let f = facts(&m, SIGN_IN);
    assert!(f.installing.is_some_and(|a| fresh(a, INSTALL_GRACE)), "{f:?}");
    assert_eq!(decide(&f), Verdict::Installing);
    m.record_agent_quit();
    assert!(facts(&m, SIGN_IN).agent_quit.is_some_and(|a| fresh(a, AGENT_QUIT_GRACE)));
    m.record_start(Some(SIGN_IN), true);
    let f = facts(&m, SIGN_IN);
    assert_eq!((f.installing, f.agent_quit), (None, None), "a start ends both");
    assert_eq!(decide(&f), Verdict::Start);

    m.record_start(None, false);
    assert_eq!(facts(&m, SIGN_IN).started, Mark::Absent, "a start in a sign-in that cannot be told names none");
    assert!(!m.path(&format!("{QUIT}.part")).exists() && !m.path(&format!("{STARTED}.part")).exists(), "written whole");
}

/// A marker's age comes from when it was written; a marker that is there but cannot be read holds
/// a person's Quit.
#[test]
fn a_marker_s_age_is_when_it_was_written() {
    let (_dir, m) = markers();
    m.record_install("0.1.0-ultra.99");
    let written = std::fs::metadata(m.path(INSTALLING)).unwrap().modified().unwrap();
    assert_eq!(m.age(INSTALLING, written + Duration::from_secs(700)), Some(700));
    assert_eq!(m.age(INSTALLING, written - Duration::from_secs(30)), Some(-30));
    assert_eq!(m.age(AGENT_QUIT, written), None);
    assert_eq!(m.read(INSTALLING).as_deref(), Some("0.1.0-ultra.99"));
    std::fs::create_dir_all(m.path(QUIT)).unwrap();
    assert_eq!(m.read(QUIT).as_deref(), Some(""), "there, but no file to read");
    assert_eq!(facts(&m, SIGN_IN).quit, Mark::ThisSignIn);
}

#[test]
fn installers_are_told_by_name() {
    for name in ["SlopBar-0.1.0-ultra.42-installer.exe", "SlopBar_0.1.0-ultra.42_x64-setup.exe", "slopbar_0.2.57_x64-setup.exe"] {
        assert!(is_installer(name), "{name}");
    }
    for name in ["slopbar.exe", "slopbar-mcp.exe", "slopbar-mcp.old-20261024-120000-42.exe", "setup.exe", "Other_x64-setup.exe", "uninstall.exe"] {
        assert!(!is_installer(name), "{name}");
    }
}

#[test]
fn the_watchdog_s_commands_come_from_the_second_argument() {
    let args = |a: &[&str]| -> Vec<std::ffi::OsString> { a.iter().map(Into::into).collect() };
    assert_eq!(command(&args(&["slopbar.exe", FLAG])), Some(Command::Check));
    assert_eq!(command(&args(&["slopbar.exe", FLAG, "extra"])), Some(Command::Check), "never a second launch");
    assert_eq!(command(&args(&["slopbar.exe", REMOVE_FLAG])), Some(Command::Remove));
    for other in [&["slopbar.exe"][..], &["slopbar.exe", "--after", "42"], &["slopbar.exe", STARTED_FLAG, "42"], &["slopbar.exe", "x", FLAG]] {
        assert_eq!(command(&args(other)), None, "{other:?}");
    }
    assert_eq!(started_by(&args(&["slopbar.exe", STARTED_FLAG, "4242"])), Some(4242));
    assert_eq!(started_by(&args(&["slopbar.exe", STARTED_FLAG])), Some(0), "a watchdog start whatever its id");
    assert_eq!(started_by(&args(&["slopbar.exe", STARTED_FLAG, "x"])), Some(0));
    assert_eq!(started_by(&args(&["slopbar.exe", "--after", "4242"])), None);
    assert_eq!(started_by(&args(&["slopbar.exe"])), None);
}

// ------------------------------------------------------------------------------------------------
// The task.

fn expected() -> Shape {
    shape(&task_xml(Path::new(EXE))).expect("the app's own definition reads")
}

/// The task's definition: it runs this app with the flag, as the signed-in user's interactive token
/// (only while signed in, no password), every 5 minutes for ever, never twice at once, with no
/// execution time limit, on batteries too.
#[test]
fn the_task_runs_this_app_every_5_minutes_while_signed_in() {
    let s = expected();
    assert_eq!(s.actions, [(EXE.to_string(), FLAG.to_string())], "this app, with the flag, and nothing else");
    assert_eq!((s.principals, s.user.as_deref()), (1, None));
    assert_eq!(s.logon, "InteractiveToken", "only while the user is signed in, as /IT does");
    assert_eq!(s.run_level, "LeastPrivilege");
    assert_eq!(s.triggers, [Trigger { kind: "TimeTrigger".into(), enabled: true, interval: Some("PT5M".into()), duration: None, ends: false, time_limit: None }],
        "every 5 minutes, for ever");
    assert_eq!(s.instances, "IgnoreNew", "never a second run at once");
    assert_eq!(s.time_limit, "PT0S", "no execution time limit");
    assert!(s.starts_on_batteries && !s.stops_on_batteries && s.enabled && !s.only_if_idle && !s.only_if_network);
    assert_eq!(EVERY, Duration::from_secs(5 * 60));
    assert!(problems(&s, &s, Some(SID)).is_empty());
    assert!(task_xml(Path::new(EXE)).starts_with("<?xml version=\"1.0\" encoding=\"UTF-16\"?>"), "registered as a BSTR: UTF-16");
}

/// A task as Task Scheduler gives it back: its defaults left out, the user's SID added. It reads as
/// this app's task.
const REGISTERED: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>SlopBar Ultra</Author>
    <Description>Starts SlopBar Ultra again.</Description>
    <URI>\SlopBar Ultra keep running S-1-5-21-1000-1000-1000-1001</URI>
  </RegistrationInfo>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-21-1000-1000-1000-1001</UserId>
      <LogonType>InteractiveToken</LogonType>
    </Principal>
  </Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
  </Settings>
  <Triggers>
    <TimeTrigger>
      <StartBoundary>2026-01-01T00:00:00</StartBoundary>
      <Repetition>
        <Interval>PT5M</Interval>
      </Repetition>
    </TimeTrigger>
  </Triggers>
  <Actions Context="Author">
    <Exec>
      <Command>C:\Users\synthetic-user\AppData\Local\SlopBar\slopbar.exe</Command>
      <Arguments>--keep-running</Arguments>
    </Exec>
  </Actions>
</Task>
"#;

#[test]
fn a_registered_task_reads_with_its_defaults() {
    let found = shape(REGISTERED).unwrap();
    assert_eq!(found.user.as_deref(), Some(SID));
    assert_eq!(problems(&found, &expected(), Some(SID)), Vec::<String>::new());
    assert_eq!(judge(Some(REGISTERED), &expected(), Some(SID)), Judged::Ok);
    assert_eq!(judge(Some(&REGISTERED.replace('\n', "\r\n\r\n")), &expected(), Some(SID)), Judged::Ok, "as schtasks prints it");
    assert_eq!(judge(Some(&REGISTERED.replace(r"\SlopBar\slopbar.exe", r"\SLOPBAR\SlopBar.EXE")), &expected(), Some(SID)), Judged::Ok, "a path in another case");
    assert_eq!(judge(Some(&REGISTERED.replace("<Command>C", "<Command>\"C").replace(".exe</Command>", ".exe\"</Command>")), &expected(), Some(SID)), Judged::Ok, "quoted");
    assert_eq!(judge(None, &expected(), Some(SID)), Judged::Missing);
}

/// Each way a registered task can differ from this app's is a problem, named, and the task is
/// repaired at the next start.
#[test]
fn each_change_to_the_task_is_a_problem() {
    let cases: &[(&str, &str, &str)] = &[
        (r"\SlopBar\slopbar.exe</Command>", r"\Old\slopbar.exe</Command>", "runs slopbar.exe --keep-running, not this app's --keep-running"),
        ("<Arguments>--keep-running</Arguments>", "<Arguments></Arguments>", "runs slopbar.exe, not this app's --keep-running"),
        ("<Arguments>--keep-running</Arguments>", "<Arguments>--keep-running --verbose</Arguments>", "runs slopbar.exe --keep-running --verbose"),
        ("</Exec>\n", "</Exec>\n    <Exec><Command>C:\\x\\other.exe</Command></Exec>\n", "runs slopbar.exe --keep-running and other.exe"),
        ("<LogonType>InteractiveToken</LogonType>", "<LogonType>Password</LogonType>", "logon type Password, not InteractiveToken"),
        ("<LogonType>InteractiveToken</LogonType>", "<LogonType>S4U</LogonType>", "logon type S4U, not InteractiveToken"),
        ("<LogonType>InteractiveToken</LogonType>", "<LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel>", "run level HighestAvailable"),
        ("<UserId>S-1-5-21-1000-1000-1000-1001</UserId>", "<UserId>S-1-5-21-1000-1000-1000-1002</UserId>", "runs as another user"),
        ("<Interval>PT5M</Interval>", "<Interval>PT10M</Interval>", "triggers: TimeTrigger every PT10M"),
        ("<Interval>PT5M</Interval>", "<Interval>PT5M</Interval><Duration>P1D</Duration>", "triggers: TimeTrigger every PT5M for P1D"),
        ("</Repetition>", "</Repetition><EndBoundary>2027-01-01T00:00:00</EndBoundary>", "with an end"),
        ("</Repetition>", "</Repetition><Enabled>false</Enabled>", "TimeTrigger disabled"),
        ("</Repetition>", "</Repetition><ExecutionTimeLimit>PT1H</ExecutionTimeLimit>", "limit PT1H"),
        ("<Repetition>\n        <Interval>PT5M</Interval>\n      </Repetition>", "", "TimeTrigger not repeating"),
        ("</TimeTrigger>", "</TimeTrigger><LogonTrigger><Enabled>true</Enabled></LogonTrigger>", "; LogonTrigger not repeating"),
        ("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>", "<MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>", "multiple instances: Parallel, not IgnoreNew"),
        ("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>", "", "execution time limit PT72H, not none"),
        ("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>", "<ExecutionTimeLimit>PT72H</ExecutionTimeLimit>", "execution time limit PT72H, not none"),
        ("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>", "", "held back or stopped on batteries"),
        ("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>", "", "held back or stopped on batteries"),
        ("<IdleSettings>", "<Enabled>false</Enabled><IdleSettings>", "disabled"),
        ("<IdleSettings>", "<RunOnlyIfIdle>true</RunOnlyIfIdle><IdleSettings>", "runs only when idle or on a network"),
        ("<IdleSettings>", "<RunOnlyIfNetworkAvailable>true</RunOnlyIfNetworkAvailable><IdleSettings>", "runs only when idle or on a network"),
    ];
    for (from, to, said) in cases {
        assert!(REGISTERED.contains(from), "the sample has {from:?}");
        let changed = REGISTERED.replacen(from, to, 1);
        let found = shape(&changed).unwrap_or_else(|| panic!("reads: {changed}"));
        let p = problems(&found, &expected(), Some(SID));
        assert!(p.len() == 1 && p[0].contains(said), "{from:?} -> {to:?}: {p:?}");
        assert!(matches!(judge(Some(&changed), &expected(), Some(SID)), Judged::Wrong(_)));
    }
    let two = REGISTERED.replace("</Principals>", "<Principal id=\"Other\"><LogonType>Password</LogonType></Principal></Principals>");
    assert_eq!(problems(&shape(&two).unwrap(), &expected(), Some(SID)), ["2 principals"]);
    let named = REGISTERED.replace("<UserId>S-1-5-21-1000-1000-1000-1001</UserId>", "<UserId>SYNTHETIC\\someone</UserId>");
    assert!(problems(&shape(&named).unwrap(), &expected(), Some(SID)).is_empty(), "a name says nothing either way");
    assert_eq!(judge(Some("<Task><Actions>"), &expected(), Some(SID)), Judged::Wrong(vec!["its definition cannot be read".into()]));
    assert_eq!(judge(Some("<Other/>"), &expected(), Some(SID)), Judged::Wrong(vec!["its definition cannot be read".into()]));
}

/// The reader takes what a task's definition holds, and nothing it cannot read for sure.
#[test]
fn the_definition_reader_takes_what_a_task_holds() {
    let doc = parse("<?xml version=\"1.0\"?>\n<!-- a > comment -->\n<t:Task a=\"x > y\" b='q\"r'>\n  <A>one &amp; &lt;two&gt; &quot;3&quot; &apos;4&apos; &#65;&#x42;</A>\n  <B/>\n  <C x=\"/\">c</C>\n</t:Task>\n").unwrap();
    assert_eq!(doc.name, "Task");
    assert_eq!(doc.text_of("A"), Some("one & <two> \"3\" '4' AB"));
    assert_eq!(doc.text_of("B"), Some(""));
    assert_eq!(doc.text_of("C"), Some("c"));
    assert_eq!(doc.children.len(), 3);
    for bad in ["", "<a>", "<a></b>", "<a><b></a></b>", "<a>&bogus;</a>", "<a>&amp</a>", "<a/><b/>", "<a><![CDATA[x]]></a>", "<!DOCTYPE a><a/>", "<a x=\"y>", "</a>", "<>x</>"] {
        assert_eq!(parse(bad), None, "{bad:?}");
    }
}

/// A path with characters XML must escape is written escaped and read back as it is.
#[test]
fn a_path_xml_must_escape_reads_back_as_it_is() {
    let exe = r#"C:\Users\Zoë & O'Neil <x>\AppData\Local\SlopBar\slopbar.exe"#;
    let xml = task_xml(Path::new(exe));
    assert!(xml.contains("Zoë &amp; O&apos;Neil &lt;x&gt;"));
    assert_eq!(shape(&xml).unwrap().actions, [(exe.to_string(), FLAG.to_string())]);
    assert_eq!(unescape(&escape(exe)).as_deref(), Some(exe));
}

/// Only Ultra release builds keep the task: a debug build is a console app, which would show a
/// console window at every run, and other builds take no Ultra releases. Tests are debug builds.
#[test]
fn only_ultra_release_builds_keep_the_task() {
    assert!(builds_task(true, false));
    assert!(!builds_task(true, true));
    assert!(!builds_task(false, false));
    assert!(!builds_task(false, true));
    assert!(!manages(), "a test build keeps no task");
}

/// The app the check starts outlives it: it breaks away from a job that lets it; it stays in one
/// that neither ends nor bounds it (Task Scheduler's: no limits, no breakaway); and it is not started
/// at all into a job that would end or bound it.
#[cfg(windows)]
#[test]
fn the_app_is_never_tied_to_the_task_s_job() {
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
    assert_eq!(spawn_flags(None), Ok(0), "no job");
    assert_eq!(spawn_flags(Some(0)), Ok(0), "Task Scheduler's job");
    assert_eq!(spawn_flags(Some(JOB_OBJECT_LIMIT_BREAKAWAY_OK)), Ok(CREATE_BREAKAWAY_FROM_JOB));
    assert_eq!(spawn_flags(Some(JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)), Ok(CREATE_BREAKAWAY_FROM_JOB));
    assert_eq!(spawn_flags(Some(JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)), Ok(0), "children leave it by themselves");
    assert_eq!(spawn_flags(Some(JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION)), Ok(0), "a limit that neither ends nor bounds it");
    for tying in [JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_PROCESS_TIME, JOB_OBJECT_LIMIT_JOB_MEMORY,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_ACTIVE_PROCESS] {
        assert_eq!(spawn_flags(Some(tying)), Err(()), "{tying:#x}");
    }
}

/// Synthetic PE headers: the subsystem field tells a window app (2) from a console app (3).
#[test]
fn a_window_app_is_told_from_a_console_app() {
    let header = |magic: u16, subsystem: u16| {
        let mut b = vec![0u8; 0x200];
        b[..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        let optional = 0x80 + 4 + 20;
        b[optional..optional + 2].copy_from_slice(&magic.to_le_bytes());
        b[optional + 68..optional + 70].copy_from_slice(&subsystem.to_le_bytes());
        b
    };
    assert_eq!(subsystem(&header(0x20b, 2)), Some(2), "PE32+ window app");
    assert_eq!(subsystem(&header(0x10b, 2)), Some(2), "PE32 window app");
    assert_eq!(subsystem(&header(0x20b, 3)), Some(3), "console app");
    assert_eq!(subsystem(&header(0x999, 2)), None, "no optional header");
    let mut no_pe = header(0x20b, 2);
    no_pe[0x80] = b'X';
    assert_eq!(subsystem(&no_pe), None);
    assert_eq!(subsystem(b"MZ"), None, "cut short");
    assert_eq!(subsystem(&header(0x20b, 2)[..0x80 + 4 + 20 + 69]), None, "cut short in the field");
    // This test binary is a console app, as debug builds are.
    #[cfg(windows)]
    assert!(!is_window_app(&std::env::current_exe().unwrap()));
}

/// Task Scheduler gives times as OLE dates in local time; get_app_info shows them in UTC.
#[test]
fn task_scheduler_dates_read_as_utc() {
    use chrono::TimeZone;
    assert_eq!(ole_date(0.0), None, "a task that has not run");
    assert_eq!(ole_date(f64::NAN), None);
    let local = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).unwrap().and_hms_opt(19, 48, 15).unwrap();
    let days = (local - chrono::NaiveDate::from_ymd_opt(1899, 12, 30).unwrap().and_hms_opt(0, 0, 0).unwrap()).num_milliseconds() as f64 / 86_400_000.0;
    let want = chrono::Local.from_local_datetime(&local).earliest().unwrap().with_timezone(&chrono::Utc);
    assert_eq!(ole_date(days), Some(want));
}

/// The graces fit what they wait for: an install's handover and its installer, the manual install
/// route, and launch at login; and each spans at least one run of the task.
#[test]
fn the_graces_fit_what_they_wait_for() {
    assert!(INSTALL_GRACE > crate::update_check::HANDOFF_LIMIT + Duration::from_secs(5 * 60), "the handover, then the installer");
    assert!(INSTALL_GRACE >= 2 * EVERY && AGENT_QUIT_GRACE >= 2 * EVERY && SIGN_IN_GRACE >= EVERY);
    assert_eq!(AGENT_QUIT_GRACE, Duration::from_secs(15 * 60), "the owner's 15 minutes for the manual install route");
}

#[test]
fn keep_running_is_a_setting_that_starts_on() {
    assert!(Settings::default().keep_running);
    let older: Settings = serde_json::from_str("{\"launch_at_login\": true}").unwrap();
    assert!(older.keep_running, "settings saved before it read it as on");
    let off = crate::settings::patched(&Settings::default(), &serde_json::json!({ "keep_running": false }), false).unwrap();
    assert!(!off.keep_running);
    assert!(crate::settings::patched(&Settings::default(), &serde_json::json!({ "keep_running": "no" }), false).is_err());
}

// ------------------------------------------------------------------------------------------------
// Where the app writes what the check reads. app.rs and main.rs need a running app; these read
// their source as the other guards do (`source_guard`).

const MAIN: &str = "keep_running::tests::main_rs_runs_the_check_before_the_instance_claim (src-tauri/src/keep_running/tests.rs)";
const QUITS: &str = "keep_running::tests::the_tray_quit_and_quit_app_write_their_markers (src-tauri/src/keep_running/tests.rs)";
const HANDOVER: &str = "keep_running::tests::the_update_handover_writes_the_install_marker_before_it_hands_over (src-tauri/src/keep_running/tests.rs)";
const WIRED: &str = "keep_running::tests::the_app_keeps_the_task_and_reports_it (src-tauri/src/keep_running/tests.rs)";
const UNINSTALL: &str = "keep_running::tests::the_uninstaller_removes_the_task_and_updates_keep_it (src-tauri/src/keep_running/tests.rs)";

/// main.rs runs the check (and the uninstaller's removal) before the instance claim, which hands a
/// second launch over to the running app by opening Settings there; a watchdog start claims as a
/// restart does; and every start that wins the claim is recorded, saying whether the watchdog made
/// it, before the app runs. Release builds are window apps.
#[test]
fn main_rs_runs_the_check_before_the_instance_claim() {
    let main = tokens(include_str!("../main.rs"));
    assert_eq!(find(&main, "#![cfg_attr(not(debug_assertions), windows_subsystem = \"windows\")]"), Some(0),
        "release builds are window apps, so the task's runs show no console. {}", update(MAIN));
    let body = function(&main, "main.rs", "fn main() {", MAIN);
    let at = |needle: &str| find(body, needle).unwrap_or_else(|| panic!("{}\n{body}", lost("main() in main.rs", needle, MAIN)));
    let check = at("if let Some(code) = slopbar_lib::keep_running(&args) { std::process::exit(code); }");
    let watchdog = at("let watchdog = slopbar_lib::watchdog_start(&args);");
    let after = at("let after = watchdog.or_else(");
    let claim = at("if !slopbar_lib::claim_instance(after) { return; }");
    let recorded = at("slopbar_lib::keep_running_started(watchdog.is_some());");
    let run = at("slopbar_lib::run()");
    assert!(check < watchdog && watchdog < after && after < claim && claim < recorded && recorded < run,
        "the check, then the claim (a watchdog start's as a restart's), then the start recorded, then the app. {}\n{body}", update(MAIN));
    assert_eq!((words(body, "claim_instance"), words(body, "keep_running_started")), (1, 1), "{}", update(MAIN));
}

/// The tray's (and the widget's) Quit writes a person's Quit before it exits; quit_app writes its
/// own marker, and restart_app neither; nothing else writes them.
#[test]
fn the_tray_quit_and_quit_app_write_their_markers() {
    let app = tokens(include_str!("../app.rs"));
    let menu = function(&app, "app.rs", "pub(crate) fn menu_event(", QUITS);
    let quit = "\"quit\" => { crate::keep_running::quit_by_person(); app.exit(0) }";
    assert!(has(menu, quit), "{}\n{menu}", lost("menu_event in app.rs", quit, QUITS));
    let door = function(&app, "app.rs", "R::QuitApp | R::RestartApp => {", QUITS);
    let split = find(door, "} else {").unwrap_or_else(|| panic!("{}\n{door}", lost("quit_app in app.rs", "} else {", QUITS)));
    let (restart, quit) = door.split_at(split);
    assert!(has(restart, "if restart { self::restart(&app)?;") && !has(restart, "keep_running::"), "restart_app writes no marker. {}\n{door}", update(QUITS));
    assert!(quit.starts_with(&tokens("} else { crate::keep_running::quit_by_agent();")), "quit_app writes its marker first. {}\n{door}", update(QUITS));
    assert_eq!((words(&app, "quit_by_person"), words(&app, "quit_by_agent")), (1, 1), "written there only. {}", update(QUITS));
}

/// The update's handover writes the install marker after the install is recorded and announced and
/// right before it hands over to the installer; nothing else writes it.
#[test]
fn the_update_handover_writes_the_install_marker_before_it_hands_over() {
    let code = tokens(include_str!("../updates.rs"));
    let install = function(&code, "updates.rs", "async fn install_now(", HANDOVER);
    let at = |needle: &str| find(install, needle).unwrap_or_else(|| panic!("{}\n{install}", lost("install_now in updates.rs", needle, HANDOVER)));
    let marked = at("let _ = app.emit(\"update-installing\", &update.version); crate::keep_running::installing(&update.version); update_check::hand_over(");
    assert!(at("record(app, by, Outcome::Installing(") < marked, "{}", update(HANDOVER));
    assert_eq!(words(&code, "installing"), 1, "written there only. {}", update(HANDOVER));
}

/// Every settings change, and startup, reach the task (`apply`); get_app_info reports it.
#[test]
fn the_app_keeps_the_task_and_reports_it() {
    let app = tokens(include_str!("../app.rs"));
    let apply = function(&app, "app.rs", "pub(crate) fn apply_settings(", WIRED);
    let wired = "crate::keep_running::apply(settings.keep_running);";
    assert!(has(apply, wired), "{}\n{apply}", lost("apply_settings in app.rs", wired, WIRED));
    let info = function(&app, "app.rs", "R::GetAppInfo => Ok(serde_json::json!({", WIRED);
    assert!(has(info, "\"keep_running\": keep_running_report(&app).await"), "{}\n{info}", update(WIRED));
    let report = function(&app, "app.rs", "async fn keep_running_report(", WIRED);
    assert!(has(report, "spawn_blocking(move || crate::keep_running::report(enabled))"), "off the async threads. {}\n{report}", update(WIRED));
}

/// The uninstaller asks the app to remove its task before anything else (the check could otherwise
/// start SlopBar while files go); an update keeps it.
#[test]
fn the_uninstaller_removes_the_task_and_updates_keep_it() {
    let hooks = include_str!("../../nsis/ultra-hooks.nsh").replace("\r\n", "\n");
    let start = hooks.find("!macro NSIS_HOOK_PREUNINSTALL").unwrap_or_else(|| panic!("ultra-hooks.nsh has no NSIS_HOOK_PREUNINSTALL. {}", update(UNINSTALL)));
    let body = &hooks[start..start + hooks[start..].find("!macroend").expect("the macro ends")];
    let lines: Vec<&str> = body.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with(';')).collect();
    assert_eq!(lines, [
        "!macro NSIS_HOOK_PREUNINSTALL",
        "${If} $UpdateMode <> 1",
        "ExecWait '\"$INSTDIR\\slopbar.exe\" --remove-keep-running'",
        "ClearErrors",
        "${EndIf}",
    ], "{}", update(UNINSTALL));
    assert_eq!(REMOVE_FLAG, "--remove-keep-running");
}

// ------------------------------------------------------------------------------------------------
// The live check: registers a task under a test name for a given executable, reads it back, and
// removes it unless asked to keep it (to let it run). Never the real task's name.
//
//   $env:SLOPBAR_KEEP_RUNNING_LIVE_TASK = 'SlopBarKeepRunningTest-<anything>'
//   $env:SLOPBAR_KEEP_RUNNING_LIVE_EXE = '<a release build>\slopbar.exe'
//   $env:SLOPBAR_KEEP_RUNNING_LIVE_KEEP = '1'   # optional
//   cargo test --lib keep_running::tests::live_task_round_trip -- --ignored --exact

#[cfg(windows)]
#[test]
#[ignore = "registers a scheduled task; run by hand with a test name"]
fn live_task_round_trip() {
    let name = std::env::var("SLOPBAR_KEEP_RUNNING_LIVE_TASK").expect("a test task name");
    assert!(name.starts_with("SlopBarKeepRunningTest-") && !name.starts_with(TASK_NAME), "a test name only: {name}");
    let exe = std::path::PathBuf::from(std::env::var("SLOPBAR_KEEP_RUNNING_LIVE_EXE").expect("an executable"));
    assert!(is_window_app(&exe), "the live check runs a window app (a release build)");
    let xml = task_xml(&exe);
    scheduler::register(&name, &xml).unwrap();
    let registered = scheduler::read(&name).unwrap().expect("registered");
    println!("{}", registered.xml);
    assert_eq!(judge(Some(&registered.xml), &shape(&xml).unwrap(), sys::user_sid().as_deref()), Judged::Ok);
    if std::env::var_os("SLOPBAR_KEEP_RUNNING_LIVE_KEEP").is_none() {
        assert!(scheduler::delete(&name).unwrap());
        assert!(scheduler::read(&name).unwrap().is_none());
        assert!(!scheduler::delete(&name).unwrap(), "gone");
    }
}
