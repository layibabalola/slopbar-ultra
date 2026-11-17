use super::*;
use crate::source_guard::{find, function, has, lost, tokens, update, words};
use crate::test_support::TempDir;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;

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
        shutting_down: false,
        enabled: true,
        uninstalling: None,
        sign_in_age: Some(HOUR),
        quit: Mark::Absent,
        started: Mark::ThisSignIn,
        launches_at_login: true,
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

/// A running SlopBar ends the check first, whatever else holds.
#[test]
fn a_running_slopbar_comes_first() {
    let held = [
        Facts { enabled: false, ..stopped() },
        Facts { shutting_down: true, ..stopped() },
        Facts { uninstalling: Some(0), ..stopped() },
        Facts { sign_in_age: None, ..stopped() },
        Facts { quit: Mark::ThisSignIn, installer: true, installing: Some(0), agent_quit: Some(0), ..stopped() },
        Facts { started: Mark::Absent, launches_at_login: false, ..stopped() },
        Facts::default(),
    ];
    for facts in held {
        assert_eq!(decide(&Facts { running: true, ..facts.clone() }), Verdict::Running, "{facts:?}");
    }
}

/// Windows signing out or shutting down, the setting off, an uninstall, or a sign-in that cannot be
/// told (so neither a person's Quit nor SlopBar's last start can be read against it) start nothing,
/// in that order.
#[test]
fn what_holds_the_check_before_the_markers() {
    assert_eq!(decide(&Facts { shutting_down: true, enabled: false, ..stopped() }), Verdict::ShuttingDown);
    assert_eq!(decide(&Facts { enabled: false, uninstalling: Some(0), ..stopped() }), Verdict::Off);
    assert_eq!(decide(&Facts { uninstalling: Some(0), sign_in_age: None, ..stopped() }), Verdict::Uninstalling);
    assert_eq!(decide(&Facts { uninstalling: Some(UNINSTALL_GRACE.as_secs() as i64), ..stopped() }), Verdict::Start, "an uninstall that failed lets go");
    assert_eq!(decide(&Facts { sign_in_age: None, quit: Mark::ThisSignIn, ..stopped() }), Verdict::UnknownSignIn);
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

/// A sign-in SlopBar has not started in: nothing stopped, so the check starts it only where Windows
/// starts it at sign-in, and only once the startup entry has had its time; a sign-in it ran in is
/// restarted at once.
#[test]
fn a_sign_in_slopbar_never_ran_in() {
    let young = SIGN_IN_GRACE - Duration::from_secs(1);
    for started in [Mark::Absent, Mark::EarlierSignIn, Mark::ThisSignIn] {
        for launches_at_login in [false, true] {
            for age in [Duration::ZERO, young, SIGN_IN_GRACE, HOUR] {
                let facts = Facts { started, launches_at_login, sign_in_age: Some(age), ..stopped() };
                let want = if started == Mark::ThisSignIn {
                    Verdict::Start
                } else if !launches_at_login {
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

/// Every time limit, exactly: the owner's 5 minutes and 15 minutes, and the rest as reasoned where
/// they are defined.
#[test]
fn the_limits_are_pinned() {
    assert_eq!(EVERY, Duration::from_secs(5 * 60));
    assert_eq!(SIGN_IN_GRACE, Duration::from_secs(5 * 60));
    assert_eq!(INSTALL_GRACE, Duration::from_secs(10 * 60));
    assert_eq!(AGENT_QUIT_GRACE, Duration::from_secs(15 * 60));
    assert_eq!(UNINSTALL_GRACE, Duration::from_secs(60 * 60));
    assert_eq!(HANDSHAKE_GRACE, Duration::from_secs(2 * 60));
    assert_eq!(SIGN_IN_START_WINDOW, Duration::from_secs(30 * 60));
    assert_eq!(SCHEDULER_LIMIT, Duration::from_secs(10));
    assert_eq!(SHELL_LIMIT, Duration::from_secs(30));
    assert_eq!(RETRIES, [Duration::from_secs(60), Duration::from_secs(5 * 60), Duration::from_secs(15 * 60)]);
    assert_eq!(PRIORITY, 5);
    assert_eq!(START_BOUNDARY, "2026-01-01T00:00:00");
    assert!(INSTALL_GRACE > crate::update_check::HANDOFF_LIMIT + Duration::from_secs(5 * 60), "the handover, then the installer");
    assert!(SCHEDULER_LIMIT < crate::door::CLIENT_TIMEOUT, "get_app_info answers before its client gives up");
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
}

/// The sign-in from WTSINFO's numbers: the session and its logon time name it, and its age is
/// counted in FILETIME's 100 ns from the logon to now.
#[test]
fn a_sign_in_is_its_session_and_logon_time() {
    let logon = 134391268890553375i64;
    let s = sign_in_from(1, logon, logon + 36_000_000_000).unwrap();
    assert_eq!(s.id, SIGN_IN);
    assert_eq!(s.age, HOUR, "an hour is 36,000,000,000 ticks of 100 ns");
    assert_eq!(sign_in_from(1, logon, logon + 7).unwrap().age, Duration::from_nanos(700));
    assert_eq!(sign_in_from(1, logon, logon - 5).unwrap().age, Duration::ZERO, "a clock behind the logon");
    assert_ne!(sign_in_from(1, logon, logon).unwrap().id, sign_in_from(1, logon + 1, logon + 1).unwrap().id, "the next sign-in, same session");
    assert_ne!(sign_in_from(1, logon, logon).unwrap().id, sign_in_from(2, logon, logon).unwrap().id);
    assert_eq!(sign_in_from(1, 0, logon), None, "no logon time");
}

// ------------------------------------------------------------------------------------------------
// The startup entry.

#[test]
fn a_startup_entry_s_program_is_read_from_its_command() {
    assert_eq!(run_program(&format!("{EXE} ")), Some(PathBuf::from(EXE)), "as tauri-plugin-autostart writes it, with a space after");
    assert_eq!(run_program(&format!("\"{EXE}\" --flag")), Some(PathBuf::from(EXE)));
    assert_eq!(run_program(r"C:\Program Files\X\slopbar.EXE --flag"), Some(PathBuf::from(r"C:\Program Files\X\slopbar.EXE")));
    assert_eq!(run_program("rundll32 something"), None);
    assert_eq!(run_program("\"\""), None);
}

/// What Windows does with the entry at sign-in: a policy blocks every user entry; then the entry
/// must be there, start this app, and not be disabled in Task Manager (an odd first byte).
#[test]
fn the_startup_entry_is_judged_as_windows_runs_it() {
    let this = |p: &Path| p == Path::new(EXE);
    let command = format!("{EXE} ");
    let enabled = [2u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let disabled = [3u8, 0, 0, 0, 0xf7, 0x37, 0xd7, 0x46, 0xe1, 0xfc, 0xd6, 1];
    assert_eq!(run_entry_from(Some(&command), Some(&enabled), false, this), RunEntry::Ok);
    assert_eq!(run_entry_from(Some(&command), None, false, this), RunEntry::Ok, "never touched in Task Manager");
    assert_eq!(run_entry_from(Some(&command), Some(&disabled), false, this), RunEntry::Disabled);
    assert_eq!(run_entry_from(Some(&command), Some(&[7u8][..]), false, this), RunEntry::Disabled);
    assert_eq!(run_entry_from(Some(&command), Some(&[6u8][..]), false, this), RunEntry::Ok);
    assert_eq!(run_entry_from(None, Some(&enabled), false, this), RunEntry::Missing);
    assert_eq!(run_entry_from(Some(r"C:\Old\slopbar.exe"), Some(&enabled), false, this), RunEntry::Elsewhere);
    assert_eq!(run_entry_from(Some("garbled"), None, false, this), RunEntry::Elsewhere);
    assert_eq!(run_entry_from(Some(&command), Some(&enabled), true, this), RunEntry::Blocked);
    assert_eq!(run_entry_from(None, None, true, this), RunEntry::Blocked);
    for entry in [RunEntry::Ok, RunEntry::Missing, RunEntry::Elsewhere, RunEntry::Disabled, RunEntry::Blocked] {
        assert_eq!(launches_at_login(true, entry), entry == RunEntry::Ok, "{entry:?}");
        assert!(!launches_at_login(false, entry));
    }
    assert_eq!([RunEntry::Ok, RunEntry::Missing, RunEntry::Elsewhere, RunEntry::Disabled, RunEntry::Blocked].map(RunEntry::name), ["ok", "missing", "elsewhere", "disabled", "blocked"]);
}

#[test]
fn the_same_file_is_told_whatever_its_spelling() {
    let dir = TempDir::new();
    let a = dir.0.join("slopbar.exe");
    std::fs::write(&a, "x").unwrap();
    std::fs::write(dir.0.join("other.exe"), "x").unwrap();
    assert!(same_file(&a, &PathBuf::from(a.to_string_lossy().to_uppercase())));
    assert!(same_file(&a, &dir.0.join(".").join("slopbar.exe")));
    assert!(!same_file(&a, &dir.0.join("other.exe")));
    let elsewhere = dir.0.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("slopbar.exe"), "x").unwrap();
    assert!(!same_file(&a, &elsewhere.join("slopbar.exe")), "another folder's slopbar.exe");
    assert!(!same_file(&a, &dir.0.join("missing.exe")));
}

/// The entry's name is the product name, which tauri-plugin-autostart names it after.
#[test]
fn the_startup_entry_is_named_after_the_product() {
    let conf: serde_json::Value = serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
    assert_eq!(conf["productName"], RUN_NAME);
}

// ------------------------------------------------------------------------------------------------
// The check, on a machine of its own.

struct FakeMachine {
    running: bool,
    shutting_down: bool,
    settings: Settings,
    sign_in: Option<SignIn>,
    installer: bool,
    entry: RunEntry,
    exe: Result<PathBuf, String>,
    now: SystemTime,
    asked: RefCell<Vec<&'static str>>,
}

impl FakeMachine {
    /// SlopBar stopped an hour into this sign-in; Windows starts it at sign-in.
    fn stopped() -> Self {
        FakeMachine {
            running: false,
            shutting_down: false,
            settings: Settings { keep_running: true, launch_at_login: true, ..Settings::default() },
            sign_in: Some(SignIn { id: SIGN_IN.into(), age: HOUR }),
            installer: false,
            entry: RunEntry::Ok,
            exe: Ok(PathBuf::from(EXE)),
            now: SystemTime::now(),
            asked: RefCell::default(),
        }
    }

    fn ask(&self, what: &'static str) {
        self.asked.borrow_mut().push(what);
    }
}

impl Machine for FakeMachine {
    fn running(&self) -> bool {
        self.ask("running");
        self.running
    }
    fn shutting_down(&self) -> bool {
        self.ask("shutting_down");
        self.shutting_down
    }
    fn settings(&self) -> Settings {
        self.ask("settings");
        self.settings.clone()
    }
    fn sign_in(&self) -> Option<SignIn> {
        self.ask("sign_in");
        self.sign_in.clone()
    }
    fn installer_running(&self) -> bool {
        self.ask("installer");
        self.installer
    }
    fn run_entry(&self, app: &Path) -> RunEntry {
        self.ask("run_entry");
        assert_eq!(Ok(app.to_path_buf()), self.exe, "the entry is judged against the app the check would start");
        self.entry
    }
    fn app_exe(&self) -> Result<PathBuf, String> {
        self.ask("app_exe");
        self.exe.clone()
    }
    fn now(&self) -> SystemTime {
        self.ask("now");
        self.now
    }
}

#[derive(Default)]
struct FakeLauncher {
    refuse: Option<Refused>,
    launched: RefCell<Vec<(PathBuf, Vec<String>, PathBuf)>>,
    /// The markers' folder: what it holds at the moment of the start.
    markers: Option<PathBuf>,
}

impl Launcher for FakeLauncher {
    fn launch(&self, exe: &Path, args: &[String], dir: &Path) -> Result<Launched, Refused> {
        if let (Some(folder), [flag, nonce]) = (&self.markers, args) {
            assert_eq!(flag, STARTED_FLAG);
            assert_eq!(Markers(folder.clone()).read(HANDSHAKE).as_deref(), Some(nonce.as_str()), "the nonce is written before the start");
        }
        self.launched.borrow_mut().push((exe.to_path_buf(), args.to_vec(), dir.to_path_buf()));
        self.refuse.clone().map_or(Ok(Launched::Shell), Err)
    }
}

fn markers() -> (TempDir, Markers) {
    let dir = TempDir::new();
    let markers = Markers(dir.0.join("keep-running"));
    (dir, markers)
}

/// The check on `machine`, with these markers: its exit code, what it launched, and what it recorded.
fn run_check(machine: &FakeMachine, markers: &Markers, refuse: Option<Refused>) -> (i32, Vec<(PathBuf, Vec<String>, PathBuf)>, Option<LastCheck>) {
    let launcher = FakeLauncher { refuse, markers: Some(markers.0.clone()), ..FakeLauncher::default() };
    let exit = check_with(machine, Some(markers), &launcher);
    (exit, launcher.launched.into_inner(), markers.last_check(SystemTime::now()))
}

/// While SlopBar runs, the check reads nothing else, starts nothing and records nothing: it ends at
/// once, and never signals the app.
#[test]
fn the_check_ends_at_once_while_slopbar_runs() {
    let (_dir, m) = markers();
    m.record_quit(Some(EARLIER)).unwrap();
    let machine = FakeMachine { running: true, ..FakeMachine::stopped() };
    let (exit, launched, record) = run_check(&machine, &m, None);
    assert_eq!((exit, launched.len(), record), (0, 0, None));
    assert_eq!(*machine.asked.borrow(), ["running"], "nothing else is read");
    assert!(m.read(HANDSHAKE).is_none());
}

/// A start: the app beside this one, in its own folder, with the flag and a fresh nonce, which is
/// written before the start; the decision and exit code are recorded.
#[test]
fn the_check_starts_the_app_with_its_handshake() {
    let (_dir, m) = markers();
    let machine = FakeMachine::stopped();
    let (exit, launched, record) = run_check(&machine, &m, None);
    assert_eq!(exit, 0);
    let [(exe, args, dir)] = &launched[..] else { panic!("one start: {launched:?}") };
    assert_eq!((exe, dir), (&PathBuf::from(EXE), &PathBuf::from(EXE).parent().unwrap().to_path_buf()));
    let nonce = m.read(HANDSHAKE).expect("the nonce, written before the start");
    assert_eq!(args, &[STARTED_FLAG.to_string(), nonce.clone()]);
    assert_eq!(nonce.len(), 32);
    assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    let record = record.unwrap();
    assert_eq!((record.decision.as_str(), record.exit), ("start", 0));
    // Each start has a nonce of its own.
    let (_, again, _) = run_check(&machine, &m, None);
    assert_ne!(again[0].1[1], nonce);
}

/// Each fact comes from where it belongs: the machine's settings, sign-in, installer list, startup
/// entry and shutdown state, and each marker. Each case changes one of them from a start, and the
/// check then decides otherwise and starts nothing.
#[test]
fn the_check_reads_each_fact_from_its_source() {
    type Change = fn(&mut FakeMachine, &Markers);
    let cases: [(&str, Change, &str); 14] = [
        ("installer", |f, _| f.installer = true, "installing"),
        ("setting off", |f, _| f.settings.keep_running = false, "off"),
        ("shutting down", |f, _| f.shutting_down = true, "shutting_down"),
        ("no sign-in", |f, _| f.sign_in = None, "unknown_sign_in"),
        ("launch at login off, never ran here", |f, m| { f.settings.launch_at_login = false; m.remove(STARTED); }, "never_started"),
        ("entry disabled in Task Manager, never ran here", |f, m| { f.entry = RunEntry::Disabled; m.remove(STARTED); }, "never_started"),
        ("entry blocked by policy, never ran here", |f, m| { f.entry = RunEntry::Blocked; m.remove(STARTED); }, "never_started"),
        ("entry missing, never ran here", |f, m| { f.entry = RunEntry::Missing; m.remove(STARTED); }, "never_started"),
        ("young sign-in, never ran here", |f, m| { f.sign_in = Some(SignIn { id: SIGN_IN.into(), age: Duration::from_secs(60) }); m.remove(STARTED); }, "sign_in_starting"),
        ("a person's Quit", |_, m| m.record_quit(Some(SIGN_IN)).unwrap(), "quit_by_person"),
        ("quit_app's marker alone", |_, m| m.record_agent_quit(), "quit_by_agent"),
        ("the install marker alone", |_, m| m.record_install("0.1.0-ultra.99"), "installing"),
        ("an uninstall", |_, m| m.write(UNINSTALLING, "uninstalling").unwrap(), "uninstalling"),
        ("a start recorded in an earlier sign-in, no launch at login", |f, m| { f.settings.launch_at_login = false; m.write(STARTED, EARLIER).unwrap(); }, "never_started"),
    ];
    for (case, change, decision) in cases {
        let (_dir, m) = markers();
        m.write(STARTED, SIGN_IN).unwrap();
        let mut machine = FakeMachine::stopped();
        change(&mut machine, &m);
        let (exit, launched, record) = run_check(&machine, &m, None);
        assert_eq!((exit, launched.len()), (0, 0), "{case}");
        assert_eq!(record.map(|r| r.decision), Some(decision.to_string()), "{case}");
    }
    // And the same machine, unchanged, starts it: in a sign-in it ran in, and also in one it never
    // ran in where Windows starts it at sign-in, once the startup entry has had its time.
    for never_ran in [false, true] {
        let (_dir, m) = markers();
        if !never_ran {
            m.write(STARTED, SIGN_IN).unwrap();
        }
        let (exit, launched, _) = run_check(&FakeMachine::stopped(), &m, None);
        assert_eq!((exit, launched.len()), (0, 1), "never ran: {never_ran}");
    }
}

/// The exit code says how the start went: 1 when it failed, 2 when it would have been tied to the
/// task's job; a decision other than a start starts nothing, whatever it is.
#[test]
fn the_check_s_exit_code_says_how_the_start_went() {
    let (_dir, m) = markers();
    let machine = FakeMachine::stopped();
    let (exit, launched, record) = run_check(&machine, &m, Some(Refused::Failed("synthetic".into())));
    assert_eq!((exit, launched.len(), record.map(|r| (r.decision, r.exit))), (1, 1, Some(("start".into(), 1))));
    let (exit, _, record) = run_check(&machine, &m, Some(Refused::Tied));
    assert_eq!((exit, record.map(|r| r.exit)), (2, Some(2)));
    m.write(STARTED, SIGN_IN).unwrap();
    let machine = FakeMachine { exe: Err("synthetic".into()), ..FakeMachine::stopped() };
    let (exit, launched, _) = run_check(&machine, &m, None);
    assert_eq!((exit, launched.len()), (1, 0), "no app to start");
    let machine = FakeMachine { settings: Settings { launch_at_login: false, ..FakeMachine::stopped().settings }, ..FakeMachine::stopped() };
    m.remove(STARTED);
    let (exit, launched, record) = run_check(&machine, &m, None);
    assert_eq!((exit, launched.len(), record.map(|r| r.decision)), (0, 0, Some("never_started".into())));
}

#[test]
fn without_its_markers_the_check_starts_nothing() {
    let launcher = FakeLauncher::default();
    assert_eq!(check_with(&FakeMachine::stopped(), None, &launcher), 0);
    assert!(launcher.launched.borrow().is_empty());
}

// ------------------------------------------------------------------------------------------------
// How the app is started.

/// Through the shell first: Explorer's child, at Explorer's priority, in none of the task's jobs.
/// Only when the shell fails does the check start it itself, at normal priority, breaking away from
/// a job that allows it, staying in one that can neither end nor bound it, and never into one that
/// could.
#[test]
fn the_app_is_started_through_the_shell_first() {
    let spawned = RefCell::new(Vec::new());
    let spawn = |flags: u32| -> std::io::Result<()> {
        spawned.borrow_mut().push(flags);
        Ok(())
    };
    assert_eq!(launch_with(|| Ok(()), spawn, Some(0x2000)), Ok(Launched::Shell));
    assert!(spawned.borrow().is_empty(), "the shell started it");
    let no_shell = || Err::<(), ShellError>(ShellError::Unavailable("synthetic".into()));
    assert_eq!(launch_with(no_shell, spawn, None), Ok(Launched::Spawned));
    assert_eq!(spawned.take(), [NORMAL_PRIORITY_CLASS], "normal priority, whatever the check runs at");
    assert_eq!(launch_with(no_shell, spawn, Some(0)), Ok(Launched::Spawned), "Task Scheduler's job");
    assert_eq!(spawned.take(), [NORMAL_PRIORITY_CLASS]);
    assert_eq!(launch_with(no_shell, spawn, Some(JOB_OBJECT_LIMIT_BREAKAWAY_OK)), Ok(Launched::Spawned));
    assert_eq!(spawned.take(), [CREATE_BREAKAWAY_FROM_JOB | NORMAL_PRIORITY_CLASS]);
    assert_eq!(launch_with(no_shell, spawn, Some(0x2000)), Err(Refused::Tied), "kill on close, no breakaway");
    assert!(spawned.take().is_empty(), "never started into a job that could end it");
    // A breakaway refused: the same start inside a job that ties nothing; none inside one that does.
    let refuse_breakaway = |flags: u32| -> std::io::Result<()> {
        spawned.borrow_mut().push(flags);
        if flags & CREATE_BREAKAWAY_FROM_JOB != 0 { Err(std::io::Error::from_raw_os_error(5)) } else { Ok(()) }
    };
    assert_eq!(launch_with(no_shell, refuse_breakaway, Some(JOB_OBJECT_LIMIT_BREAKAWAY_OK)), Ok(Launched::Spawned));
    assert_eq!(spawned.take(), [CREATE_BREAKAWAY_FROM_JOB | NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS]);
    assert!(matches!(launch_with(no_shell, refuse_breakaway, Some(JOB_OBJECT_LIMIT_BREAKAWAY_OK | 0x2000)), Err(Refused::Failed(_))));
    assert_eq!(spawned.take(), [CREATE_BREAKAWAY_FROM_JOB | NORMAL_PRIORITY_CLASS]);
    let fails = |_flags: u32| -> std::io::Result<()> { Err(std::io::Error::from_raw_os_error(2)) };
    assert!(matches!(launch_with(no_shell, fails, None), Err(Refused::Failed(_))));
    // A shell that did not answer may still start it: nothing more is started.
    assert_eq!(launch_with(|| Err(ShellError::NoAnswer), spawn, None), Err(Refused::Failed("the shell did not answer".into())));
    assert!(spawned.take().is_empty());
}

/// The shell's call is waited for at most its limit, so a hung Explorer cannot hold the check.
#[test]
fn the_shell_is_waited_for_within_a_limit() {
    assert_eq!(bounded(Duration::from_secs(60), || Ok(())), Ok(()));
    assert_eq!(bounded(Duration::from_secs(60), || Err("no desktop".into())), Err(ShellError::Unavailable("no desktop".into())));
    let started = std::time::Instant::now();
    let (release, hung) = mpsc::channel::<()>();
    assert_eq!(bounded(Duration::from_millis(100), move || {
        let _ = hung.recv_timeout(Duration::from_secs(60));
        Ok(())
    }), Err(ShellError::NoAnswer));
    assert!(started.elapsed() < Duration::from_secs(30), "not held by the hung call");
    drop(release);
}

#[test]
fn a_job_that_could_end_or_bound_the_app_ties_it() {
    assert_eq!(spawn_flags(None), Ok(0), "no job");
    assert_eq!(spawn_flags(Some(0)), Ok(0), "Task Scheduler's job");
    assert_eq!(spawn_flags(Some(JOB_OBJECT_LIMIT_BREAKAWAY_OK)), Ok(CREATE_BREAKAWAY_FROM_JOB));
    assert_eq!(spawn_flags(Some(JOB_OBJECT_LIMIT_BREAKAWAY_OK | 0x2000)), Ok(CREATE_BREAKAWAY_FROM_JOB));
    assert_eq!(spawn_flags(Some(JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK | 0x2000)), Ok(0), "children leave it by themselves");
    assert_eq!(spawn_flags(Some(0x0400)), Ok(0), "a limit that neither ends nor bounds it (die on an unhandled exception)");
    // Kill on close, job time, process time, job memory, process memory, active processes.
    for tying in [0x2000, 0x0004, 0x0002, 0x0200, 0x0100, 0x0008] {
        assert_eq!(spawn_flags(Some(tying)), Err(()), "{tying:#x}");
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::JobObjects as job;
        use windows_sys::Win32::System::Threading as threading;
        assert_eq!(TYING, job::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | job::JOB_OBJECT_LIMIT_JOB_TIME | job::JOB_OBJECT_LIMIT_PROCESS_TIME
            | job::JOB_OBJECT_LIMIT_JOB_MEMORY | job::JOB_OBJECT_LIMIT_PROCESS_MEMORY | job::JOB_OBJECT_LIMIT_ACTIVE_PROCESS);
        assert_eq!((JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK), (job::JOB_OBJECT_LIMIT_BREAKAWAY_OK, job::JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK));
        assert_eq!((CREATE_BREAKAWAY_FROM_JOB, NORMAL_PRIORITY_CLASS), (threading::CREATE_BREAKAWAY_FROM_JOB, threading::NORMAL_PRIORITY_CLASS));
    }
}

// ------------------------------------------------------------------------------------------------
// How a start counts.

/// The startup entry's start at sign-in: no arguments, no shortcut, Explorer as the parent, within
/// SIGN_IN_START_WINDOW of the sign-in. Anything else is not.
#[test]
fn the_startup_entry_s_start_is_told_from_others() {
    let young = Some(Duration::from_secs(8 * 60));
    assert!(sign_in_start(true, false, Some("explorer.exe"), young), "a late startup entry");
    assert!(sign_in_start(true, false, Some("EXPLORER.EXE"), Some(Duration::ZERO)));
    assert!(!sign_in_start(false, false, Some("explorer.exe"), young), "arguments: the watchdog, a restart or an installer");
    assert!(!sign_in_start(true, true, Some("explorer.exe"), young), "a shortcut: the Start menu, the taskbar, the desktop");
    assert!(!sign_in_start(true, false, Some("pwsh.exe"), young), "a script");
    assert!(!sign_in_start(true, false, None, young), "a parent that is gone");
    assert!(!sign_in_start(true, false, Some("explorer.exe"), Some(SIGN_IN_START_WINDOW)), "long after the sign-in");
    assert!(!sign_in_start(true, false, Some("explorer.exe"), None), "no sign-in");
    for (start, quiet) in [(Start::Watchdog, true), (Start::SignIn, true), (Start::Other, false)] {
        assert_eq!(start.is_quiet(), quiet, "{start:?}");
    }
}

fn args(a: &[&str]) -> Vec<OsString> {
    a.iter().map(Into::into).collect()
}

/// Only a start that presents the nonce the check wrote, within HANDSHAKE_GRACE, is the watchdog's,
/// and only once: the updater restarts an app with its old arguments, whose nonce is spent.
#[test]
fn only_the_watchdog_s_own_start_counts_as_its() {
    let (_dir, m) = markers();
    let now = SystemTime::now();
    let never = || -> bool { panic!("a start with arguments is no startup entry's") };
    m.write_handshake("0123456789abcdef0123456789abcdef").unwrap();
    let watchdog = args(&["slopbar.exe", STARTED_FLAG, "0123456789abcdef0123456789abcdef"]);
    assert_eq!(start_kind_with(&watchdog, Some(&m), now, never), Start::Watchdog);
    assert_eq!(start_kind_with(&watchdog, Some(&m), now, never), Start::Other, "spent: the updater's restart with the old arguments");
    m.write_handshake("0123456789abcdef0123456789abcdef").unwrap();
    assert_eq!(start_kind_with(&args(&["slopbar.exe", STARTED_FLAG, "fedcba9876543210fedcba9876543210"]), Some(&m), now, never), Start::Other, "another nonce");
    assert_eq!(start_kind_with(&args(&["slopbar.exe", STARTED_FLAG]), Some(&m), now, never), Start::Other, "none");
    assert_eq!(start_kind_with(&watchdog, Some(&m), now + HANDSHAKE_GRACE + Duration::from_secs(2), never), Start::Other, "too old");
    assert_eq!(start_kind_with(&watchdog, None, now, never), Start::Other, "no markers");
    assert_eq!(start_kind_with(&args(&["slopbar.exe"]), Some(&m), now, || true), Start::SignIn);
    assert_eq!(start_kind_with(&args(&["slopbar.exe"]), Some(&m), now, || false), Start::Other);
    assert_eq!(start_kind_with(&args(&["slopbar.exe", "--after", "42"]), Some(&m), now, never), Start::Other);
    assert_eq!(start_kind_with(&args(&["slopbar.exe", "x", STARTED_FLAG, "0123"]), Some(&m), now, never), Start::Other, "the flag counts second only");
}

#[test]
fn the_watchdog_s_command_comes_from_the_second_argument() {
    assert_eq!(handshake_arg(&args(&["slopbar.exe", STARTED_FLAG, "abc"])), Some("abc".into()));
    assert_eq!(handshake_arg(&args(&["slopbar.exe", "x", STARTED_FLAG])), None);
    for other in [&["slopbar.exe"][..], &["slopbar.exe", "--after", "42"], &["slopbar.exe", "x", FLAG]] {
        assert_eq!(from_args(&args(other)), None, "{other:?}");
    }
}

// ------------------------------------------------------------------------------------------------
// What the app writes.

/// A person's Quit holds for the rest of its sign-in and lapses at the next; a start that is not the
/// watchdog's ends it, the watchdog's own does not; every start ends the install and quit_app
/// markers, and none ends an uninstall's.
#[test]
fn starts_and_quits_leave_what_the_check_reads() {
    let (_dir, m) = markers();
    let none = || -> Result<bool, String> { panic!("the marker was written; nothing to remove") };
    assert_eq!(quit_by_person_in(Some(&m), Some(SIGN_IN), &none), Ok(QuitHeld::Marker));
    assert_eq!(m.read(QUIT).as_deref(), Some(SIGN_IN), "naming the sign-in");
    assert!(started_in(Some(&m), Some(SIGN_IN), Start::Watchdog), "the watchdog's");
    assert_eq!(m.read(QUIT).as_deref(), Some(SIGN_IN), "the watchdog's own start ends no Quit");
    assert!(!started_in(Some(&m), Some(SIGN_IN), Start::SignIn));
    assert_eq!(m.read(QUIT), None, "any other start does");
    assert_eq!(m.read(STARTED).as_deref(), Some(SIGN_IN));
    quit_by_person_in(Some(&m), None, &none).unwrap();
    assert_eq!(m.read(QUIT).as_deref(), Some(""), "a Quit naming no sign-in, which holds");
    assert!(!started_in(Some(&m), None, Start::Other));
    assert_eq!((m.read(QUIT), m.read(STARTED)), (None, None), "no sign-in to name");

    installing_in(Some(&m), "0.1.0-ultra.99");
    assert_eq!((m.read(INSTALLING).as_deref(), m.read(AGENT_QUIT), m.read(QUIT)), (Some("0.1.0-ultra.99"), None, None), "the install marker only");
    quit_by_agent_in(Some(&m));
    assert!(m.read(AGENT_QUIT).is_some() && m.read(QUIT).is_none(), "quit_app's marker, never a person's Quit");
    m.write(UNINSTALLING, "uninstalling").unwrap();
    started_in(Some(&m), Some(SIGN_IN), Start::Watchdog);
    assert_eq!((m.read(INSTALLING), m.read(AGENT_QUIT)), (None, None), "a start ends both");
    assert!(m.read(UNINSTALLING).is_some(), "but not an uninstall's hold");
    assert!(!m.path(&format!("{QUIT}.part")).exists() && !m.path(&format!("{STARTED}.part")).exists(), "written whole");
    // Without markers nothing is written, and nothing breaks.
    assert!(!started_in(None, Some(SIGN_IN), Start::Other));
    quit_by_agent_in(None);
    installing_in(None, "x");
}

/// A Quit whose marker cannot be written (twice) is held by removing the task, so no check runs at
/// all; a start that is not the watchdog's registers it again. One that cannot be held either way
/// says so.
#[test]
fn a_quit_that_cannot_be_written_removes_the_task() {
    let dir = TempDir::new();
    let blocked = dir.0.join("not-a-folder");
    std::fs::write(&blocked, "a file where the markers' folder would be").unwrap();
    let m = Markers(blocked);
    let removed = AtomicUsize::new(0);
    let remove = || -> Result<bool, String> {
        removed.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    };
    assert_eq!(quit_by_person_in(Some(&m), Some(SIGN_IN), &remove), Ok(QuitHeld::NoTask));
    assert_eq!(removed.load(Ordering::SeqCst), 1);
    assert_eq!(quit_by_person_in(None, Some(SIGN_IN), &remove), Ok(QuitHeld::NoTask), "no markers' folder at all");
    let fails = || -> Result<bool, String> { Err("synthetic".into()) };
    assert_eq!(quit_by_person_in(Some(&m), Some(SIGN_IN), &fails), Err("synthetic".into()));
}

/// The markers are the machine's own (the local data folder), never the roaming one.
#[test]
fn the_markers_are_machine_local() {
    assert_eq!(Markers::here().map(|m| m.0), crate::settings::local_dir().map(|d| d.join("keep-running")));
    assert_ne!(crate::settings::local_dir(), crate::settings::app_dir());
}

/// A marker's age comes from when it was written; a marker that is there but cannot be read holds
/// a person's Quit; the last check reads back as written.
#[test]
fn a_marker_s_age_is_when_it_was_written() {
    let (_dir, m) = markers();
    m.record_install("0.1.0-ultra.99");
    let written = std::fs::metadata(m.path(INSTALLING)).unwrap().modified().unwrap();
    assert_eq!(m.age(INSTALLING, written + Duration::from_secs(700)), Some(700));
    assert_eq!(m.age(INSTALLING, written - Duration::from_secs(30)), Some(-30));
    assert_eq!(m.age(AGENT_QUIT, written), None);
    std::fs::create_dir_all(m.path(QUIT)).unwrap();
    assert_eq!(m.read(QUIT).as_deref(), Some(""), "there, but no file to read");
    assert_eq!(quit_mark(m.read(QUIT).as_deref(), SIGN_IN), Mark::ThisSignIn);
    m.record_check(Verdict::Start, 2);
    let check = m.last_check(SystemTime::now()).unwrap();
    assert_eq!((check.decision.as_str(), check.exit), ("start", 2));
    assert!(m.uninstalling(SystemTime::now()) == false);
    m.write(UNINSTALLING, "uninstalling").unwrap();
    assert!(m.uninstalling(SystemTime::now()));
    assert!(!m.uninstalling(SystemTime::now() + UNINSTALL_GRACE));
}

#[test]
fn installers_are_told_by_name() {
    for name in ["SlopBar-0.1.0-ultra.42-installer.exe", "SlopBar_0.1.0-ultra.42_x64-setup.exe", "slopbar_0.2.57_x64-setup.exe"] {
        assert!(is_installer(name), "{name}");
    }
    for name in ["slopbar.exe", "slopbar-mcp.exe", "slopbar-mcp.old-20261024-120000-42.exe", "setup.exe", "Other_x64-setup.exe", "uninstall.exe"] {
        assert!(!is_installer(name), "{name}");
    }
    assert_eq!(beside(Path::new(r"C:\x\slopbar.old-1.exe")), PathBuf::from(format!(r"C:\x\{}", crate::instance::APP_EXE)));
}

// ------------------------------------------------------------------------------------------------
// The task's definition.

fn expected() -> Shape {
    shape(&task_xml(Path::new(EXE))).expect("the app's own definition reads")
}

/// The task's definition: it runs this app with the flag, as the signed-in user's interactive token
/// (only while signed in, no password), every 5 minutes for ever from a start in the past, never
/// twice at once, with no execution time limit, at normal priority, on batteries too, without waking
/// the computer, and can be run on demand.
#[test]
fn the_task_runs_this_app_every_5_minutes_while_signed_in() {
    let s = expected();
    assert_eq!(s.actions, [(EXE.to_string(), FLAG.to_string())], "this app, with the flag, and nothing else");
    assert!(s.context_ok, "its actions run as its principal");
    assert_eq!((s.principals, s.user.as_deref()), (1, None));
    assert_eq!(s.logon, "InteractiveToken", "only while the user is signed in, as /IT does");
    assert_eq!(s.run_level, "LeastPrivilege");
    let trigger = Trigger { kind: "TimeTrigger".into(), enabled: true, start: Some(START_BOUNDARY.into()), interval: Some("PT5M".into()), duration: None, ends: false, time_limit: None };
    assert_eq!(s.triggers, [trigger], "every 5 minutes, for ever, from a start in the past");
    assert_eq!(s.instances, "IgnoreNew", "never a second run at once");
    assert_eq!(s.time_limit, "PT0S", "no execution time limit");
    assert_eq!(s.priority, 5, "normal priority");
    assert!(s.starts_on_batteries && !s.stops_on_batteries && s.enabled && !s.only_if_idle && !s.only_if_network);
    assert!(!s.wakes, "never wakes the computer");
    assert!(s.on_demand);
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
    <Priority>5</Priority>
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
/// repaired at the next start; a task a person turned off is not (it is `Judged::Disabled`).
#[test]
fn each_change_to_the_task_is_a_problem() {
    let cases: &[(&str, &str, &str)] = &[
        (r"\SlopBar\slopbar.exe</Command>", r"\Old\slopbar.exe</Command>", "runs slopbar.exe --keep-running, not this app's --keep-running"),
        ("<Arguments>--keep-running</Arguments>", "<Arguments></Arguments>", "runs slopbar.exe, not this app's --keep-running"),
        ("<Arguments>--keep-running</Arguments>", "<Arguments>--keep-running --verbose</Arguments>", "runs slopbar.exe --keep-running --verbose"),
        ("</Exec>\n", "</Exec>\n    <Exec><Command>C:\\x\\other.exe</Command></Exec>\n", "runs slopbar.exe --keep-running and other.exe"),
        ("<Actions Context=\"Author\">", "<Actions Context=\"Nobody\">", "its actions name no principal of its own"),
        ("<LogonType>InteractiveToken</LogonType>", "<LogonType>Password</LogonType>", "logon type Password, not InteractiveToken"),
        ("<LogonType>InteractiveToken</LogonType>", "<LogonType>S4U</LogonType>", "logon type S4U, not InteractiveToken"),
        ("<LogonType>InteractiveToken</LogonType>", "<LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel>", "run level HighestAvailable"),
        ("<UserId>S-1-5-21-1000-1000-1000-1001</UserId>", "<UserId>S-1-5-21-1000-1000-1000-1002</UserId>", "runs as another user"),
        ("<Interval>PT5M</Interval>", "<Interval>PT10M</Interval>", "triggers: TimeTrigger every PT10M"),
        ("<Interval>PT5M</Interval>", "<Interval>PT5M</Interval><Duration>P1D</Duration>", "triggers: TimeTrigger every PT5M for P1D"),
        ("<StartBoundary>2026-01-01T00:00:00</StartBoundary>", "<StartBoundary>2099-01-01T00:00:00</StartBoundary>", "from 2099-01-01T00:00:00"),
        ("</Repetition>", "</Repetition><EndBoundary>2027-01-01T00:00:00</EndBoundary>", "with an end"),
        ("</Repetition>", "</Repetition><ExecutionTimeLimit>PT1H</ExecutionTimeLimit>", "limit PT1H"),
        ("<Repetition>\n        <Interval>PT5M</Interval>\n      </Repetition>", "", "TimeTrigger not repeating"),
        ("</TimeTrigger>", "</TimeTrigger><LogonTrigger><Enabled>true</Enabled></LogonTrigger>", "; LogonTrigger not repeating"),
        ("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>", "<MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy>", "multiple instances: Parallel, not IgnoreNew"),
        ("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>", "", "execution time limit PT72H, not none"),
        ("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>", "<ExecutionTimeLimit>PT72H</ExecutionTimeLimit>", "execution time limit PT72H, not none"),
        ("<Priority>5</Priority>", "", "priority 7, not 5"),
        ("<Priority>5</Priority>", "<Priority>7</Priority>", "priority 7, not 5"),
        ("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>", "", "held back or stopped on batteries"),
        ("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>", "", "held back or stopped on batteries"),
        ("<IdleSettings>", "<RunOnlyIfIdle>true</RunOnlyIfIdle><IdleSettings>", "runs only when idle or on a network"),
        ("<IdleSettings>", "<RunOnlyIfNetworkAvailable>true</RunOnlyIfNetworkAvailable><IdleSettings>", "runs only when idle or on a network"),
        ("<IdleSettings>", "<WakeToRun>true</WakeToRun><IdleSettings>", "wakes the computer"),
        ("<IdleSettings>", "<AllowStartOnDemand>false</AllowStartOnDemand><IdleSettings>", "cannot be run on demand"),
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

/// A task (or its trigger) a person turned off in Task Scheduler is theirs: judged disabled, never
/// a problem to repair, whatever else it holds.
#[test]
fn a_task_turned_off_by_hand_is_left_off() {
    let off = REGISTERED.replace("<Priority>5</Priority>", "<Priority>5</Priority><Enabled>false</Enabled>");
    assert_eq!(judge(Some(&off), &expected(), Some(SID)), Judged::Disabled);
    let trigger_off = REGISTERED.replace("</Repetition>", "</Repetition><Enabled>false</Enabled>");
    assert_eq!(judge(Some(&trigger_off), &expected(), Some(SID)), Judged::Disabled);
    let off_and_old = off.replace("<Priority>5</Priority>", "<Priority>7</Priority>");
    assert_eq!(judge(Some(&off_and_old), &expected(), Some(SID)), Judged::Disabled, "off wins over a repair");
    assert!(problems(&shape(&trigger_off).unwrap(), &expected(), Some(SID)).is_empty(), "turned off is no problem");
}

/// The reader takes what a task's definition holds, and nothing it cannot read for sure.
#[test]
fn the_definition_reader_takes_what_a_task_holds() {
    let doc = parse("<?xml version=\"1.0\"?>\n<!-- a > comment -->\n<t:Task a=\"x > y\" b='q\"r'>\n  <A>one &amp; &lt;two&gt; &quot;3&quot; &apos;4&apos; &#65;&#x42;</A>\n  <B/>\n  <C x=\"/\">c</C>\n</t:Task>\n").unwrap();
    assert_eq!(doc.name, "Task");
    assert_eq!((doc.attribute("a"), doc.attribute("b")), (Some("x > y"), Some("q\"r")));
    assert_eq!(doc.text_of("A"), Some("one & <two> \"3\" '4' AB"));
    assert_eq!(doc.text_of("B"), Some(""));
    assert_eq!((doc.text_of("C"), doc.child("C").unwrap().attribute("x")), (Some("c"), Some("/")));
    assert_eq!(doc.children.len(), 3);
    for bad in ["", "<a>", "<a></b>", "<a><b></a></b>", "<a>&bogus;</a>", "<a>&amp</a>", "<a/><b/>", "<a><![CDATA[x]]></a>", "<!DOCTYPE a><a/>", "<a x=\"y>", "</a>", "<>x</>", "<a x=y/>"] {
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

/// Only Ultra release builds keep the task, and not on SLOPBAR_DATA_DIR: a debug build is a console
/// app, other builds take no Ultra releases, an isolated run leaves the installed app's task alone.
#[test]
fn only_ultra_release_builds_keep_the_task() {
    for ultra in [false, true] {
        for debug in [false, true] {
            for isolated in [false, true] {
                assert_eq!(builds_task(ultra, debug, isolated), ultra && !debug && !isolated, "{ultra} {debug} {isolated}");
            }
        }
    }
    assert!(!manages(), "a test build keeps no task");
}

/// Synthetic PE headers: the subsystem field tells a window app (2) from a console app (3).
fn pe(magic: u16, subsystem: u16) -> Vec<u8> {
    let mut b = vec![0u8; 0x200];
    b[..2].copy_from_slice(b"MZ");
    b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    b[0x80..0x84].copy_from_slice(b"PE\0\0");
    let optional = 0x80 + 4 + 20;
    b[optional..optional + 2].copy_from_slice(&magic.to_le_bytes());
    b[optional + 68..optional + 70].copy_from_slice(&subsystem.to_le_bytes());
    b
}

#[test]
fn a_window_app_is_told_from_a_console_app() {
    assert_eq!(subsystem(&pe(0x20b, 2)), Some(2), "PE32+ window app");
    assert_eq!(subsystem(&pe(0x10b, 2)), Some(2), "PE32 window app");
    assert_eq!(subsystem(&pe(0x20b, 3)), Some(3), "console app");
    assert_eq!(subsystem(&pe(0x999, 2)), None, "no optional header");
    let mut no_pe = pe(0x20b, 2);
    no_pe[0x80] = b'X';
    assert_eq!(subsystem(&no_pe), None);
    assert_eq!(subsystem(b"MZ"), None, "cut short");
    assert_eq!(subsystem(&pe(0x20b, 2)[..0x80 + 4 + 20 + 69]), None, "cut short in the field");
    // This test binary is a console app, as debug builds are.
    #[cfg(windows)]
    assert!(!is_window_app(&std::env::current_exe().unwrap()));
}

/// Task Scheduler gives times as OLE dates in local time; get_app_info shows them in UTC, whatever
/// the zone of the machine the test runs on.
#[test]
fn task_scheduler_dates_read_as_utc() {
    use chrono::TimeZone;
    assert_eq!(ole_date_in(0.0, &chrono::Utc), None, "a task that has not run");
    assert_eq!(ole_date_in(f64::NAN, &chrono::Utc), None);
    // 2026-11-16 19:48:15 is day 46342 and 71295 seconds.
    let days = 46342.0 + 71295.0 / 86400.0;
    let utc = |h, m, s| chrono::Utc.with_ymd_and_hms(2026, 11, 16, h, m, s).unwrap();
    assert_eq!(ole_date_in(days, &chrono::Utc), Some(utc(19, 48, 15)));
    assert_eq!(ole_date_in(days, &chrono::FixedOffset::west_opt(6 * 3600).unwrap()), Some(utc(19, 48, 15) + chrono::Duration::hours(6)), "six hours behind UTC");
    assert_eq!(ole_date_in(days, &chrono::FixedOffset::east_opt(5 * 3600 + 1800).unwrap()), Some(utc(14, 18, 15)), "five and a half ahead");
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
// Task Scheduler, faked.

#[derive(Default)]
struct FakeScheduler {
    tasks: Mutex<HashMap<String, Registered>>,
    log: Mutex<Vec<String>>,
    /// Registrations that fail before one succeeds.
    fail_registers: AtomicUsize,
    fail_deletes: AtomicBool,
    fail_reads: AtomicBool,
}

impl FakeScheduler {
    fn with(name: &str, xml: &str) -> Self {
        let s = FakeScheduler::default();
        lock(&s.tasks).insert(name.into(), Registered { xml: xml.into(), last_run: 0.0, last_result: 0x41303 });
        s
    }
    fn log(&self) -> Vec<String> {
        lock(&self.log).clone()
    }
    fn xml(&self, name: &str) -> Option<String> {
        lock(&self.tasks).get(name).map(|t| t.xml.clone())
    }
}

impl Scheduler for FakeScheduler {
    fn read(&self, name: &str) -> Result<Option<Registered>, String> {
        lock(&self.log).push(format!("read {name}"));
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err("synthetic read failure".into());
        }
        Ok(lock(&self.tasks).get(name).cloned())
    }
    fn register(&self, name: &str, xml: &str) -> Result<(), String> {
        lock(&self.log).push(format!("register {name}"));
        if self.fail_registers.load(Ordering::SeqCst) > 0 {
            self.fail_registers.fetch_sub(1, Ordering::SeqCst);
            return Err("synthetic register failure".into());
        }
        lock(&self.tasks).insert(name.into(), Registered { xml: xml.into(), last_run: 0.0, last_result: 0x41303 });
        Ok(())
    }
    fn delete(&self, name: &str) -> Result<bool, String> {
        lock(&self.log).push(format!("delete {name}"));
        if self.fail_deletes.load(Ordering::SeqCst) {
            return Err("synthetic delete failure".into());
        }
        Ok(lock(&self.tasks).remove(name).is_some())
    }
}

/// A task environment whose executable is a synthetic window app (or console app) in `dir`.
fn env(dir: &TempDir, window_app: bool) -> TaskEnv {
    let exe = dir.0.join("slopbar.exe");
    std::fs::write(&exe, pe(0x20b, if window_app { 2 } else { 3 })).unwrap();
    TaskEnv { name: Some(task_name_for(SID)), sid: Some(SID.into()), exe: Ok(exe), uninstalling: false }
}

fn name() -> String {
    task_name_for(SID)
}

/// Registering: a missing task is made; one as it should be is left; one that differs (or cannot be
/// read, or is someone else's) is removed and made anew; one a person turned off is left off; off
/// removes it; nothing is registered for a console app, during an uninstall, or without a SID.
#[test]
fn the_task_is_kept_as_it_should_be() {
    let dir = TempDir::new();
    let e = env(&dir, true);
    let xml = task_xml(e.exe.as_ref().unwrap());
    let n = name();
    assert!(n.ends_with(SID) && n.starts_with(TASK_NAME));

    let s = FakeScheduler::default();
    assert_eq!(sync(&s, &e, true), Ok(Synced::Registered));
    assert_eq!(s.xml(&n).as_deref(), Some(xml.as_str()));
    assert_eq!(s.log(), [format!("read {n}"), format!("register {n}")]);

    let s = FakeScheduler::with(&n, &xml);
    assert_eq!(sync(&s, &e, true), Ok(Synced::AlreadyOk));
    assert_eq!(s.log(), [format!("read {n}")], "nothing written");

    for wrong in [xml.replace("<Priority>5</Priority>", "<Priority>7</Priority>"), "corrupt".to_string(), xml.replace(&n, "x").replace("<Arguments>--keep-running", "<Arguments>")] {
        let s = FakeScheduler::with(&n, &wrong);
        assert_eq!(sync(&s, &e, true), Ok(Synced::Repaired), "{wrong}");
        assert_eq!(s.log(), [format!("read {n}"), format!("delete {n}"), format!("register {n}")], "removed, then made anew");
        assert_eq!(s.xml(&n).as_deref(), Some(xml.as_str()));
    }

    let off = xml.replace("<Enabled>true</Enabled>\n    <Hidden>", "<Enabled>false</Enabled>\n    <Hidden>");
    assert_ne!(off, xml);
    let s = FakeScheduler::with(&n, &off);
    assert_eq!(sync(&s, &e, true), Ok(Synced::LeftDisabled));
    assert_eq!(s.xml(&n).as_deref(), Some(off.as_str()), "a person's choice");

    let s = FakeScheduler::with(&n, &xml);
    assert_eq!(sync(&s, &e, false), Ok(Synced::Removed));
    assert_eq!((s.log(), s.xml(&n)), (vec![format!("delete {n}")], None), "off removes it, and registers nothing");

    let s = FakeScheduler::default();
    assert_eq!(sync(&s, &TaskEnv { uninstalling: true, ..env(&dir, true) }, true), Ok(Synced::HeldByUninstall));
    assert!(s.log().is_empty(), "nothing during an uninstall");
    let console = env(&dir, false);
    assert!(sync(&s, &console, true).unwrap_err().contains("not a window app"));
    assert!(s.log().is_empty(), "never a console app");
    assert!(sync(&s, &TaskEnv { name: None, ..env(&dir, true) }, true).is_err());
    let s = FakeScheduler::with(&n, "corrupt");
    s.fail_deletes.store(true, Ordering::SeqCst);
    assert!(sync(&s, &e, true).is_err(), "a task that cannot be removed is not registered over");
    assert!(!s.log().iter().any(|l| l.starts_with("register")));
}

/// Settings reach the task only in a build that keeps it; the value asked for is the one applied;
/// a failure is retried after each delay, a bounded number of times; a newer value takes over a
/// retry at once; a value applied once is not applied again.
#[test]
fn applying_the_setting_retries_a_failure_within_a_bound() {
    static ZERO: [Duration; 3] = [Duration::ZERO; 3];
    let calls = Arc::new(Mutex::new(Vec::new()));
    let work = |fail: Arc<AtomicUsize>, calls: Arc<Mutex<Vec<bool>>>| -> SyncWork {
        Arc::new(move |enabled| {
            lock(&calls).push(enabled);
            if fail.load(Ordering::SeqCst) > 0 {
                fail.fetch_sub(1, Ordering::SeqCst);
                return Err("synthetic".into());
            }
            Ok(if enabled { Synced::Registered } else { Synced::Removed })
        })
    };

    let syncer = Syncer::new();
    apply_with(false, &syncer, true, work(Arc::default(), calls.clone()), &ZERO);
    assert!(syncer.wait_idle(Duration::from_secs(60)));
    assert!(lock(&calls).is_empty(), "a build that keeps no task touches none");

    apply_with(true, &syncer, false, work(Arc::default(), calls.clone()), &ZERO);
    assert!(syncer.wait_idle(Duration::from_secs(60)));
    assert_eq!(*lock(&calls), [false], "the value asked for");
    assert_eq!(syncer.outcome(), Some(Ok(Synced::Removed)));
    apply_with(true, &syncer, false, work(Arc::default(), calls.clone()), &ZERO);
    assert!(syncer.wait_idle(Duration::from_secs(60)));
    assert_eq!(*lock(&calls), [false], "applied once");

    lock(&calls).clear();
    apply_with(true, &syncer, true, work(Arc::new(AtomicUsize::new(2)), calls.clone()), &ZERO);
    assert!(syncer.wait_idle(Duration::from_secs(60)));
    assert_eq!(*lock(&calls), [true, true, true], "two failures, then success");
    assert_eq!(syncer.outcome(), Some(Ok(Synced::Registered)));

    let syncer = Syncer::new();
    lock(&calls).clear();
    apply_with(true, &syncer, true, work(Arc::new(AtomicUsize::new(usize::MAX)), calls.clone()), &ZERO);
    assert!(syncer.wait_idle(Duration::from_secs(60)));
    assert_eq!(lock(&calls).len(), 1 + ZERO.len(), "once, then once after each delay, and no more");
    assert!(matches!(syncer.outcome(), Some(Err(_))));
    lock(&calls).clear();
    apply_with(true, &syncer, true, work(Arc::default(), calls.clone()), &ZERO);
    assert!(syncer.wait_idle(Duration::from_secs(60)));
    assert_eq!(*lock(&calls), [true], "a failed value is tried again when asked again");

    // A retry waiting a long time takes a newer value at once.
    static LONG: [Duration; 1] = [Duration::from_secs(3600)];
    let syncer = Syncer::new();
    lock(&calls).clear();
    let fail = Arc::new(AtomicUsize::new(1));
    apply_with(true, &syncer, true, work(fail, calls.clone()), &LONG);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while lock(&calls).is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(syncer.syncing(), "waiting to retry");
    apply_with(true, &syncer, false, work(Arc::default(), calls.clone()), &LONG);
    assert!(syncer.wait_idle(Duration::from_secs(60)), "the newer value did not wait an hour");
    assert_eq!(*lock(&calls), [true, false]);
}

/// Task Scheduler's calls run one at a time, in order: a registration whose caller stopped waiting
/// still runs before a later removal, so it can never make the task again after it.
#[test]
fn a_slow_registration_cannot_outlast_a_later_removal() {
    #[derive(Default)]
    struct Fake(HashMap<String, String>);
    impl Backend for Fake {
        fn read(&mut self, name: &str) -> Result<Option<Registered>, String> {
            Ok(self.0.get(name).map(|x| Registered { xml: x.clone(), last_run: 0.0, last_result: 0 }))
        }
        fn register(&mut self, name: &str, xml: &str) -> Result<(), String> {
            self.0.insert(name.into(), xml.into());
            Ok(())
        }
        fn delete(&mut self, name: &str) -> Result<bool, String> {
            Ok(self.0.remove(name).is_some())
        }
    }
    let queue: Queue<Fake> = Queue::new(Fake::default, Duration::from_millis(100));
    let (release, gate) = mpsc::channel::<()>();
    let slow = queue.call(move |b: &mut Fake| {
        let _ = gate.recv_timeout(Duration::from_secs(60));
        b.register("t", "x")
    });
    assert!(slow.unwrap_err().contains("did not answer within"), "its caller stopped waiting, the registration still blocked");
    // The removal comes after, and its caller gives up too while the registration still blocks.
    assert!(Scheduler::delete(&queue, "t").unwrap_err().contains("did not answer within"));
    release.send(()).unwrap();
    // Patiently: the registration ran, then the removal.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(present) = queue.call(|b: &mut Fake| b.0.contains_key("t")) {
            assert!(!present, "the task stayed removed");
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the queue never answered");
    }
    assert_eq!(Scheduler::read(&queue, "t").ok(), Some(None));
}

/// Removing the task says when it failed; without a SID it cannot be named.
#[test]
fn removing_the_task_says_how_it_went() {
    let s = FakeScheduler::with(&name(), "x");
    assert_eq!(remove_task(&s, Some(name())), Ok(true));
    assert_eq!(remove_task(&s, Some(name())), Ok(false), "gone already");
    s.fail_deletes.store(true, Ordering::SeqCst);
    assert_eq!(remove_task(&s, Some(name())), Err("synthetic delete failure".into()));
    assert!(remove_task(&s, None).is_err());
}

// ------------------------------------------------------------------------------------------------
// What get_app_info reports.

fn inputs(dir: &TempDir) -> ReportInputs {
    ReportInputs {
        enabled: true,
        managed: true,
        syncing: false,
        outcome: None,
        env: env(dir, true),
        started_by_watchdog: false,
        last_check: None,
        launch_at_login: true,
        run_entry: RunEntry::Ok,
    }
}

fn report_of(s: &FakeScheduler, inputs: &ReportInputs) -> serde_json::Value {
    report_with(s, inputs, &chrono::Utc)
}

/// The report, key by key (the fleet report reads them by name).
#[test]
fn the_report_says_how_the_task_stands() {
    let dir = TempDir::new();
    let i = inputs(&dir);
    let xml = task_xml(i.env.exe.as_ref().unwrap());
    let checked = SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_800_000);
    let s = FakeScheduler::with(&name(), &xml);
    let report = report_of(&s, &ReportInputs { started_by_watchdog: true, last_check: Some(LastCheck { at: checked, decision: "start".into(), exit: 0 }), ..inputs(&dir) });
    assert_eq!(report, serde_json::json!({
        "enabled": true,
        "managed": true,
        "task": "ok",
        "problems": [],
        "error": null,
        "last_run": null,
        "last_result": 0x41303,
        "last_check": { "at": "2026-11-16T03:33:20Z", "decision": "start", "exit": 0 },
        "launch_at_login": { "setting": true, "entry": "ok" },
        "started_by_watchdog": true,
    }));

    let missing = FakeScheduler::default();
    assert_eq!(report_of(&missing, &i)["task"], "missing");
    assert_eq!(report_of(&missing, &ReportInputs { syncing: true, ..inputs(&dir) })["task"], "pending", "the startup sync is still making it");
    let r = report_of(&missing, &ReportInputs { outcome: Some(Err("synthetic register failure".into())), ..inputs(&dir) });
    assert_eq!((&r["task"], &r["error"]), (&"missing".into(), &"synthetic register failure".into()));
    let wrong = FakeScheduler::with(&name(), &xml.replace("<Priority>5</Priority>", "<Priority>7</Priority>"));
    let r = report_of(&wrong, &i);
    assert_eq!((&r["task"], &r["problems"]), (&"wrong".into(), &serde_json::json!(["priority 7, not 5"])));
    let off = FakeScheduler::with(&name(), &xml.replace("<Enabled>true</Enabled>\n    <Hidden>", "<Enabled>false</Enabled>\n    <Hidden>"));
    assert_eq!(report_of(&off, &i)["task"], "disabled");
    let unreadable = FakeScheduler::default();
    unreadable.fail_reads.store(true, Ordering::SeqCst);
    let r = report_of(&unreadable, &i);
    assert_eq!((&r["task"], &r["error"]), (&"unknown".into(), &"synthetic read failure".into()));
    let r = report_of(&missing, &ReportInputs { launch_at_login: false, run_entry: RunEntry::Disabled, ..inputs(&dir) });
    assert_eq!(r["launch_at_login"], serde_json::json!({ "setting": false, "entry": "disabled" }));
}

/// The report's JSON, read by the fleet report's own Get-Watchdog (scripts/fleet-slopbar.ps1), says
/// what the fleet report shows for each state.
#[cfg(windows)]
#[test]
fn the_fleet_report_reads_the_report() {
    let Some(pwsh) = ["pwsh.exe", "pwsh"].iter().find(|p| std::process::Command::new(p).arg("-v").output().is_ok()) else {
        assert!(std::env::var_os("CI").is_none(), "pwsh is on every CI runner");
        eprintln!("skipped: no pwsh");
        return;
    };
    let dir = TempDir::new();
    let xml = task_xml(env(&dir, true).exe.as_ref().unwrap());
    let ok = FakeScheduler::with(&name(), &xml);
    let missing = FakeScheduler::default();
    let wrong = FakeScheduler::with(&name(), &xml.replace("<Priority>5</Priority>", "<Priority>7</Priority>"));
    let off = FakeScheduler::with(&name(), &xml.replace("<Enabled>true</Enabled>\n    <Hidden>", "<Enabled>false</Enabled>\n    <Hidden>"));
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_794_800_000);
    let failed = |exit| Some(LastCheck { at, decision: "start".into(), exit });
    let cases: Vec<(&str, serde_json::Value, &str, Vec<&str>)> = vec![
        ("ok", report_of(&ok, &inputs(&dir)), "ok", vec![]),
        ("missing", report_of(&missing, &inputs(&dir)), "missing", vec!["watchdog task missing"]),
        ("pending", report_of(&missing, &ReportInputs { syncing: true, ..inputs(&dir) }), "pending", vec![]),
        ("wrong", report_of(&wrong, &inputs(&dir)), "wrong", vec!["watchdog task wrong: priority 7, not 5"]),
        ("disabled", report_of(&off, &inputs(&dir)), "disabled by hand", vec![]),
        ("off", report_of(&missing, &ReportInputs { enabled: false, ..inputs(&dir) }), "off", vec![]),
        ("not managed", report_of(&missing, &ReportInputs { managed: false, launch_at_login: false, ..inputs(&dir) }), "missing", vec![]),
        ("exit 1", report_of(&ok, &ReportInputs { last_check: failed(1), ..inputs(&dir) }), "ok", vec!["watchdog could not start SlopBar (exit 1 at 2026-11-16T03:33:20Z)"]),
        ("exit 2", report_of(&ok, &ReportInputs { last_check: failed(2), ..inputs(&dir) }), "ok", vec!["watchdog would not start SlopBar in a job that could end it (exit 2 at 2026-11-16T03:33:20Z)"]),
        ("login off", report_of(&ok, &ReportInputs { launch_at_login: false, run_entry: RunEntry::Missing, ..inputs(&dir) }), "ok", vec!["launch at login off"]),
        ("entry missing", report_of(&ok, &ReportInputs { run_entry: RunEntry::Missing, ..inputs(&dir) }), "ok", vec!["startup entry missing"]),
        ("entry elsewhere", report_of(&ok, &ReportInputs { run_entry: RunEntry::Elsewhere, ..inputs(&dir) }), "ok", vec!["startup entry starts another program"]),
        ("entry disabled", report_of(&ok, &ReportInputs { run_entry: RunEntry::Disabled, ..inputs(&dir) }), "ok", vec!["startup entry disabled in Task Manager"]),
        ("entry blocked", report_of(&ok, &ReportInputs { run_entry: RunEntry::Blocked, ..inputs(&dir) }), "ok", vec!["startup entries blocked by policy"]),
        ("off, entry missing", report_of(&missing, &ReportInputs { enabled: false, run_entry: RunEntry::Missing, ..inputs(&dir) }), "off", vec!["startup entry missing"]),
    ];
    for (i, (_, value, _, _)) in cases.iter().enumerate() {
        std::fs::write(dir.0.join(format!("{i:02}.json")), value.to_string()).unwrap();
    }
    let script = dir.0.join("read.ps1");
    std::fs::write(&script, "param($fleet, $dir)\n. $fleet\nGet-ChildItem -LiteralPath $dir -Filter *.json | Sort-Object Name | ForEach-Object {\n  $w = Get-Watchdog (Get-Content -Raw -LiteralPath $_.FullName | ConvertFrom-Json)\n  '{0}|{1}|{2}' -f $_.BaseName, $w.Text, (@($w.Problems) -join ';')\n}\n").unwrap();
    let fleet = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("scripts").join("fleet-slopbar.ps1");
    let out = std::process::Command::new(pwsh).args(["-NoProfile", "-NonInteractive", "-File"]).arg(&script).arg(&fleet).arg(&dir.0).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), cases.len(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    for (line, (case, _, shown, problems)) in lines.iter().zip(&cases) {
        let mut parts = line.splitn(3, '|').skip(1);
        assert_eq!((parts.next(), parts.next()), (Some(*shown), Some(problems.join(";").as_str())), "{case}: {line}");
    }
}

// ------------------------------------------------------------------------------------------------
// Restarting into another build.

#[test]
fn a_build_without_the_watchdog_is_told_by_its_sentinel() {
    let dir = TempDir::new();
    let with = dir.0.join("with.exe");
    let mut bytes = vec![b'x'; (1 << 20) - 10];
    bytes.extend_from_slice(SENTINEL.as_bytes());
    bytes.extend_from_slice(&[b'y'; 100]);
    std::fs::write(&with, &bytes).unwrap();
    assert!(handles_watchdog(&with), "across the read's chunk boundary");
    let without = dir.0.join("without.exe");
    std::fs::write(&without, vec![b'x'; 3 << 20]).unwrap();
    assert!(!handles_watchdog(&without));
    assert!(handles_watchdog(&dir.0.join("missing.exe")), "one that cannot be read cannot be started either");
    let removed = AtomicUsize::new(0);
    let remove = || -> Result<bool, String> {
        removed.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    };
    assert_eq!(before_restart_with(&with, &remove), None);
    assert_eq!(removed.load(Ordering::SeqCst), 0, "a build with the watchdog keeps the task");
    assert_eq!(before_restart_with(&without, &remove), Some(Ok(true)));
    assert_eq!(removed.load(Ordering::SeqCst), 1, "the task goes before a build without it starts");
}

// ------------------------------------------------------------------------------------------------
// Where the app reaches the watchdog. app.rs, main.rs and the thin wrappers here need a running app
// or this machine; these read their source as the other guards do (`source_guard`).

const MAIN: &str = "keep_running::tests::main_rs_runs_the_check_before_the_instance_claim (src-tauri/src/keep_running/tests.rs)";
const QUITS: &str = "keep_running::tests::the_tray_quit_and_quit_app_write_their_markers (src-tauri/src/keep_running/tests.rs)";
const HANDOVER: &str = "keep_running::tests::the_update_handover_writes_the_install_marker_before_it_hands_over (src-tauri/src/keep_running/tests.rs)";
const WIRED: &str = "keep_running::tests::the_app_keeps_the_task_and_reports_it (src-tauri/src/keep_running/tests.rs)";
const UNINSTALL: &str = "keep_running::tests::the_uninstaller_holds_the_watchdog_and_removes_its_task (src-tauri/src/keep_running/tests.rs)";
const WRAPPERS: &str = "keep_running::tests::the_wrappers_only_hand_this_machine_over (src-tauri/src/keep_running/tests.rs)";

/// main.rs runs the check before the instance claim, which hands a second launch over to the
/// running app by opening Settings there; a start by the watchdog or the startup entry claims as a
/// restart does; and every start that wins the claim is recorded, as what it was, before the app
/// runs. Release builds are window apps.
#[test]
fn main_rs_runs_the_check_before_the_instance_claim() {
    let main = tokens(include_str!("../main.rs"));
    assert_eq!(find(&main, "#![cfg_attr(not(debug_assertions), windows_subsystem = \"windows\")]"), Some(0),
        "release builds are window apps, so the task's runs show no console. {}", update(MAIN));
    let body = function(&main, "main.rs", "fn main() {", MAIN);
    let at = |needle: &str| find(body, needle).unwrap_or_else(|| panic!("{}\n{body}", lost("main() in main.rs", needle, MAIN)));
    let check = at("if let Some(code) = slopbar_lib::keep_running(&args) { std::process::exit(code); }");
    let start = at("let start = slopbar_lib::keep_running_start(&args);");
    let quiet = at("let after = after.or(start.is_quiet().then_some(0));");
    let claim = at("if !slopbar_lib::claim_instance(after) { return; }");
    let recorded = at("slopbar_lib::keep_running_started(start);");
    let run = at("slopbar_lib::run()");
    assert!(check < start && start < quiet && quiet < claim && claim < recorded && recorded < run,
        "the check, then how the start counts, the claim (quiet for the watchdog's and the startup entry's), the start recorded, the app. {}\n{body}", update(MAIN));
    assert_eq!((words(body, "claim_instance"), words(body, "keep_running_started")), (1, 1), "{}", update(MAIN));
}

/// The thin wrappers hand this machine (its Explorer, Task Scheduler, sign-in and markers' folder)
/// to the functions the tests above run on fakes, and do nothing else.
#[test]
fn the_wrappers_only_hand_this_machine_over() {
    let code = tokens(include_str!("../keep_running.rs"));
    let bodies = [
        ("fn check() -> i32 {", "fn check() -> i32 { check_with(&Windows, Markers::here().as_ref(), &ShellLauncher) }"),
        ("fn launch(&self, exe: &Path, args: &[String], dir: &Path) -> Result<Launched, Refused> {",
            "fn launch(&self, exe: &Path, args: &[String], dir: &Path) -> Result<Launched, Refused> { let (e, a, d) = (exe.to_path_buf(), args.join(\" \"), dir.to_path_buf()); launch_with(|| bounded(SHELL_LIMIT, move || sys::shell_execute(&e, &a, &d)), |flags| spawn(exe, args, dir, flags), sys::job_limits()) }"),
        ("fn running(&self) -> bool {", "fn running(&self) -> bool { crate::instance::held() }"),
        ("fn shutting_down(&self) -> bool {", "fn shutting_down(&self) -> bool { sys::shutting_down() }"),
        ("fn settings(&self) -> Settings {", "fn settings(&self) -> Settings { crate::settings::load() }"),
        ("fn sign_in(&self) -> Option<SignIn> {", "fn sign_in(&self) -> Option<SignIn> { sys::session().and_then(|s| sign_in_from(s.id, s.logon, s.now)) }"),
        ("fn installer_running(&self) -> bool {", "fn installer_running(&self) -> bool { crate::instance::names().is_some_and(|names| names.iter().any(|n| is_installer(n))) }"),
        ("fn run_entry(&self, app: &Path) -> RunEntry {", "fn run_entry(&self, app: &Path) -> RunEntry { sys::run_entry(app) }"),
        ("fn app_exe(&self) -> Result<PathBuf, String> {", "fn app_exe(&self) -> Result<PathBuf, String> { Ok(beside(&std::env::current_exe().map_err(|e| e.to_string())?)) }"),
        ("pub fn started(start: Start) {", "pub fn started(start: Start) { started_in(Markers::here().as_ref(), Windows.sign_in().map(|s| s.id).as_deref(), start); }"),
        ("pub fn quit_by_person() {", "pub fn quit_by_person() { let _ = quit_by_person_in(Markers::here().as_ref(), Windows.sign_in().map(|s| s.id).as_deref(), &|| remove_task(tasks(), task_name())); }"),
        ("pub fn quit_by_agent() {", "pub fn quit_by_agent() { quit_by_agent_in(Markers::here().as_ref()); }"),
        ("pub fn installing(version: &str) {", "pub fn installing(version: &str) { installing_in(Markers::here().as_ref(), version); }"),
        ("pub fn apply(enabled: bool) {", "pub fn apply(enabled: bool) { apply_with(manages(), syncer(), enabled, Arc::new(|enabled| sync(tasks(), &TaskEnv::here(), enabled)), &RETRIES); }"),
        ("fn manages() -> bool {", "fn manages() -> bool { builds_task(cfg!(feature = \"ultra-updates\"), cfg!(debug_assertions), crate::settings::isolated()) }"),
        ("pub fn before_restart(exe: &Path) {", "pub fn before_restart(exe: &Path) { before_restart_with(exe, &|| remove_task(tasks(), task_name())); }"),
        ("pub fn from_args(args: &[OsString]) -> Option<i32> {", "pub fn from_args(args: &[OsString]) -> Option<i32> { std::hint::black_box(SENTINEL); (args.get(1)? == FLAG).then(check) }"),
        ("pub fn start_kind(args: &[OsString]) -> Start {", "pub fn start_kind(args: &[OsString]) -> Start { start_kind_with(args, Markers::here().as_ref(), SystemTime::now(), || { sign_in_start(true, sys::started_from_shortcut(), crate::instance::parent_name().as_deref(), Windows.sign_in().map(|s| s.age)) }) }"),
        ("fn here() -> Self {", "fn here() -> Self { let sid = sys::user_sid(); TaskEnv { name: sid.as_deref().map(task_name_for), sid, exe: Windows.app_exe(), uninstalling: Markers::here().is_some_and(|m| m.uninstalling(SystemTime::now())), } }"),
    ];
    for (signature, body) in bodies {
        assert_eq!(function(&code, "keep_running.rs", signature, WRAPPERS), tokens(body), "{}", update(WRAPPERS));
    }
    // started() keeps what started_in says about the watchdog for get_app_info.
    let started = function(&code, "keep_running.rs", "fn started_in(", WRAPPERS);
    assert!(has(started, "STARTED_BY_WATCHDOG.store(start == Start::Watchdog, Ordering::Relaxed);"), "{}\n{started}", update(WRAPPERS));
    let report = function(&code, "keep_running.rs", "pub fn report(enabled: bool, launch_at_login: bool) -> serde_json::Value {", WRAPPERS);
    for wired in ["managed: manages(),", "syncing: syncer().syncing(),", "outcome: syncer().outcome(),", "started_by_watchdog: STARTED_BY_WATCHDOG.load(Ordering::Relaxed),",
        "last_check: Markers::here().and_then(|m| m.last_check(SystemTime::now())),", "launch_at_login,", "report_with(tasks(), &inputs, &chrono::Local)"] {
        assert!(has(report, wired), "{}\n{report}", lost("report() in keep_running.rs", wired, WRAPPERS));
    }
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

/// Every settings change, and startup, reach the task (`apply`); get_app_info reports it, with the
/// launch-at-login setting; restart_app looks at the build it restarts into before starting it.
#[test]
fn the_app_keeps_the_task_and_reports_it() {
    let app = tokens(include_str!("../app.rs"));
    let apply = function(&app, "app.rs", "pub(crate) fn apply_settings(", WIRED);
    let wired = "crate::keep_running::apply(settings.keep_running);";
    assert!(has(apply, wired), "{}\n{apply}", lost("apply_settings in app.rs", wired, WIRED));
    let info = function(&app, "app.rs", "R::GetAppInfo => Ok(serde_json::json!({", WIRED);
    assert!(has(info, "\"keep_running\": keep_running_report(&app).await"), "{}\n{info}", update(WIRED));
    let report = function(&app, "app.rs", "async fn keep_running_report(", WIRED);
    for needle in ["(settings.keep_running, settings.launch_at_login)", "spawn_blocking(move || crate::keep_running::report(enabled, at_login))"] {
        assert!(has(report, needle), "{}\n{report}", lost("keep_running_report in app.rs", needle, WIRED));
    }
    let restart = function(&app, "app.rs", "fn restart(app: &AppHandle) -> Result<(), String> {", WIRED);
    let at = |needle: &str| find(restart, needle).unwrap_or_else(|| panic!("{}\n{restart}", lost("restart in app.rs", needle, WIRED)));
    assert!(at("crate::keep_running::before_restart(&exe);") < at("std::process::Command::new(&exe)"), "before the new build starts. {}", update(WIRED));
}

/// The uninstaller, never on an update, first writes the marker that holds every check and the
/// task's registration, then deletes the task by its name (the user's SID read from its own token)
/// through nsExec and schtasks, which show no console; it never runs a slopbar.exe. Every install
/// removes the marker.
#[test]
fn the_uninstaller_holds_the_watchdog_and_removes_its_task() {
    let hooks = include_str!("../../nsis/ultra-hooks.nsh").replace("\r\n", "\n");
    let macro_lines = |name: &str| -> Vec<String> {
        let start = hooks.find(&format!("!macro {name}")).unwrap_or_else(|| panic!("ultra-hooks.nsh has no {name}. {}", update(UNINSTALL)));
        let body = &hooks[start..start + hooks[start..].find("!macroend").expect("the macro ends")];
        body.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with(';')).map(String::from).collect()
    };
    let un = macro_lines("NSIS_HOOK_PREUNINSTALL");
    let at = |needle: &str| un.iter().position(|l| l == needle).unwrap_or_else(|| panic!("NSIS_HOOK_PREUNINSTALL has no `{needle}`. {}\n{un:#?}", update(UNINSTALL)));
    let (update_mode, marker, write, token, sid, delete) = (
        at("${If} $UpdateMode <> 1"),
        at("!insertmacro KEEP_RUNNING_MARKERS_TO_R0"),
        at("FileOpen $R1 \"$R0\\uninstalling\" w"),
        at("System::Call 'advapi32::OpenProcessToken(p -1, i 8, *p .R1) i .R2'"),
        at("System::Call 'advapi32::ConvertSidToStringSidW(p R5, *p .R6) i .R2'"),
        at("nsExec::Exec '\"$SYSDIR\\schtasks.exe\" /Delete /TN \"${KEEP_RUNNING_TASK} $R4\" /F'"),
    );
    assert!(update_mode == 1 && update_mode < marker && marker < write && write < token && token < sid && sid < delete, "{}\n{un:#?}", update(UNINSTALL));
    assert!(!un.iter().any(|l| l.to_ascii_lowercase().contains("slopbar.exe") || l.contains("ExecWait")), "no slopbar.exe runs. {}", update(UNINSTALL));
    let preinstall = macro_lines("NSIS_HOOK_PREINSTALL");
    let marker = preinstall.iter().position(|l| l == "!insertmacro KEEP_RUNNING_MARKERS_TO_R0");
    assert!(marker.is_some_and(|m| preinstall.get(m + 1).map(String::as_str) == Some("Delete \"$R0\\uninstalling\"")), "every install ends the hold. {}\n{preinstall:#?}", update(UNINSTALL));
    let folder = macro_lines("KEEP_RUNNING_MARKERS_TO_R0");
    assert!(folder.contains(&"StrCpy $R0 \"$LOCALAPPDATA\\${BUNDLEID}\\keep-running\"".to_string()), "the markers' folder, as Markers::here names it. {}", update(UNINSTALL));
    assert_eq!(UNINSTALLING, "uninstalling");
    assert!(hooks.contains(&format!("!define KEEP_RUNNING_TASK \"{TASK_NAME}\"")), "the task's name. {}", update(UNINSTALL));
}

// ------------------------------------------------------------------------------------------------
// The live check: registers a task under a test name for a given executable, reads it back, and,
// asked to, keeps it for a while (to let it run) before removing it; the task goes however the test
// ends (`LiveTask`). Never the real task's name.
//
//   $env:SLOPBAR_KEEP_RUNNING_LIVE_TASK = 'SlopBarKeepRunningTest-<anything>'
//   $env:SLOPBAR_KEEP_RUNNING_LIVE_EXE = '<a window app>'
//   $env:SLOPBAR_KEEP_RUNNING_LIVE_KEEP_SECONDS = '900'   # optional
//   cargo test --lib keep_running::tests::live_task_round_trip -- --ignored --exact --nocapture

/// A test task that is removed when this goes, whatever happened.
struct LiveTask<'a> {
    scheduler: &'a dyn Scheduler,
    name: String,
}

impl Drop for LiveTask<'_> {
    fn drop(&mut self) {
        let gone = self.scheduler.delete(&self.name);
        println!("live task {} removed: {gone:?}", self.name);
    }
}

#[cfg(windows)]
#[test]
#[ignore = "registers a scheduled task; run by hand with a test name"]
fn live_task_round_trip() {
    let name = std::env::var("SLOPBAR_KEEP_RUNNING_LIVE_TASK").expect("a test task name");
    assert!(name.starts_with("SlopBarKeepRunningTest-") && !name.starts_with(TASK_NAME), "a test name only: {name}");
    let exe = PathBuf::from(std::env::var("SLOPBAR_KEEP_RUNNING_LIVE_EXE").expect("an executable"));
    assert!(is_window_app(&exe), "the live check runs a window app");
    let xml = task_xml(&exe);
    let scheduler = tasks();
    scheduler.register(&name, &xml).unwrap();
    let task = LiveTask { scheduler, name: name.clone() };
    let registered = scheduler.read(&name).unwrap().expect("registered");
    println!("{}", registered.xml);
    assert_eq!(judge(Some(&registered.xml), &shape(&xml).unwrap(), sys::user_sid().as_deref()), Judged::Ok);
    if let Some(seconds) = std::env::var("SLOPBAR_KEEP_RUNNING_LIVE_KEEP_SECONDS").ok().and_then(|s| s.parse().ok()) {
        println!("keeping {name} for {seconds} s");
        std::thread::sleep(Duration::from_secs(seconds));
    }
    drop(task);
    assert!(scheduler.read(&name).unwrap().is_none(), "gone");
}
