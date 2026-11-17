//! SlopBar Ultra's watchdog (fork-only; docs/ULTRA-RELEASES.md, "Keeping SlopBar Ultra running").
//!
//! A scheduled task of the signed-in user's own runs `slopbar.exe --keep-running` every 5 minutes,
//! only while that user is signed in. The check starts SlopBar Ultra again when it has stopped for
//! any reason but a person's Quit during this Windows sign-in: a crash, a kill, an agent's quit_app
//! (once its grace has passed: the manual install route quits through the door, installs and starts
//! SlopBar itself), a restart or an update that failed. It never races an installer, and it never
//! signals the running app: a plain second launch would open Settings there (`instance::claim` hands
//! over), so the check only looks at the instance lock (`instance::held`) and ends at once when it
//! is held.
//!
//! The check starts SlopBar with `--watchdog-start <its own pid>`, which claims the instance lock as
//! a restart does: it waits for the check to end, and leaves quietly to an instance that won the lock.
//! Task Scheduler runs each task in a job object; the app the check starts breaks away from it when
//! the job allows that, and otherwise stays in it only when it has no limit that could end or bound
//! the app (`spawn_flags`): such a job is never closed on the app, which then outlives the check, a
//! new run, the task's repair and its removal.
//!
//! Ultra release builds create or repair the task at startup and remove it while `keep_running` is
//! off (`apply`); the uninstaller removes it (`--remove-keep-running`, src-tauri/nsis/ultra-hooks.nsh).
//!
//! What the check reads, in the machine-local data folder's `keep-running`, each written by the app:
//! - `quit`: a person's Quit (the tray's menu or the widget's), naming the Windows sign-in it was
//!   chosen in, so it lapses by itself at the next sign-in. Every start but the watchdog's removes it.
//! - `started`: the sign-in SlopBar last started in, which tells a stop from a sign-in it never ran in.
//! - `installing`: written right before an update hands over to its installer; honoured INSTALL_GRACE.
//! - `agent-quit`: the door's quit_app; honoured AGENT_QUIT_GRACE.
//!
//! A sign-in is the Windows session's id and the time its user signed in (WTSINFO): every process of
//! the sign-in reads the same pair, elevated or not (a token's logon id differs between an admin's two
//! linked tokens), locking and unlocking or reconnecting keeps it, and the next sign-in gets a new
//! logon time even where Windows reuses the session id.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use crate::settings::Settings;

/// The task's argument: the check.
pub const FLAG: &str = "--keep-running";
/// What the check starts SlopBar with, followed by the check's own process id.
pub const STARTED_FLAG: &str = "--watchdog-start";
/// The uninstaller's: removes this user's task.
pub const REMOVE_FLAG: &str = "--remove-keep-running";
/// The task's name, followed by the user's SID: task names are the machine's, the task is the user's.
pub const TASK_NAME: &str = "SlopBar Ultra keep running";
/// How often the task runs the check.
pub const EVERY: Duration = Duration::from_secs(5 * 60);
/// How long an update's install marker holds the check off. The handover runs the exit cleanup
/// (within HANDOFF_LIMIT) and the installer then takes well under a minute; an install that never
/// started SlopBar again has failed by then, and the check starts it.
pub const INSTALL_GRACE: Duration = Duration::from_secs(10 * 60);
/// How long the door's quit_app holds the check off: the manual install route quits through the
/// door, installs and starts SlopBar itself within it (docs/ULTRA-RELEASES.md).
pub const AGENT_QUIT_GRACE: Duration = Duration::from_secs(15 * 60);
/// How long a new sign-in is left to "launch at login" before the check starts SlopBar itself:
/// both at once would be two starts, and the losing one would open Settings.
pub const SIGN_IN_GRACE: Duration = Duration::from_secs(5 * 60);
/// A marker this far ahead of the clock still counts as just written (a clock set back a little).
const CLOCK_SLACK_SECONDS: i64 = 60;
/// Where the task's time trigger starts repeating from; any time in the past does.
const START_BOUNDARY: &str = "2026-01-01T00:00:00";
/// How long Task Scheduler gets to answer: the door's get_app_info waits for it, and so does the
/// uninstaller.
const SCHEDULER_LIMIT: Duration = Duration::from_secs(10);

const QUIT: &str = "quit";
const STARTED: &str = "started";
const INSTALLING: &str = "installing";
const AGENT_QUIT: &str = "agent-quit";

/// What a marker says about the current sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mark {
    #[default]
    Absent,
    ThisSignIn,
    EarlierSignIn,
}

/// What the check knows when it decides.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    /// Another process holds SlopBar's instance lock: SlopBar runs.
    pub running: bool,
    /// The `keep_running` setting.
    pub enabled: bool,
    /// How long ago this Windows sign-in began; None when the sign-in cannot be told.
    pub sign_in_age: Option<Duration>,
    /// A person's Quit.
    pub quit: Mark,
    /// SlopBar's last start.
    pub started: Mark,
    pub launch_at_login: bool,
    /// An installer of SlopBar runs.
    pub installer: bool,
    /// How many seconds ago the install marker was written (negative: ahead of the clock).
    pub installing: Option<i64>,
    /// How many seconds ago quit_app's marker was written.
    pub agent_quit: Option<i64>,
}

/// What the check does: start SlopBar, or leave it, for this reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Start,
    Running,
    Off,
    UnknownSignIn,
    QuitByPerson,
    Installing,
    QuitByAgent,
    /// SlopBar has not run in this sign-in and does not launch at login: nothing stopped.
    NeverStarted,
    /// A new sign-in, which launch at login starts SlopBar in.
    SignInStarting,
}

/// The check's choice. A running SlopBar comes first, so the check ends at once on it.
pub fn decide(f: &Facts) -> Verdict {
    if f.running {
        return Verdict::Running;
    }
    if !f.enabled {
        return Verdict::Off;
    }
    let Some(age) = f.sign_in_age else { return Verdict::UnknownSignIn };
    if f.quit == Mark::ThisSignIn {
        return Verdict::QuitByPerson;
    }
    if f.installer || f.installing.is_some_and(|a| fresh(a, INSTALL_GRACE)) {
        return Verdict::Installing;
    }
    if f.agent_quit.is_some_and(|a| fresh(a, AGENT_QUIT_GRACE)) {
        return Verdict::QuitByAgent;
    }
    if f.started != Mark::ThisSignIn {
        if !f.launch_at_login {
            return Verdict::NeverStarted;
        }
        if age < SIGN_IN_GRACE {
            return Verdict::SignInStarting;
        }
    }
    Verdict::Start
}

/// Whether a marker written `age` seconds ago still holds for `grace`. One further ahead of the
/// clock than a little was not written just now, whatever its time says, so it holds nothing.
pub fn fresh(age: i64, grace: Duration) -> bool {
    age >= -CLOCK_SLACK_SECONDS && age < grace.as_secs() as i64
}

/// A person's Quit, read against this sign-in. One that cannot be read holds, as one from this
/// sign-in would: what it says is unknown, and a person's Quit is never overridden on a guess.
fn quit_mark(text: Option<&str>, sign_in: &str) -> Mark {
    match text.map(str::trim) {
        None => Mark::Absent,
        Some(t) if is_sign_in(t) && t != sign_in => Mark::EarlierSignIn,
        Some(_) => Mark::ThisSignIn,
    }
}

/// SlopBar's last start, read against this sign-in. One that cannot be read is no start here.
fn started_mark(text: Option<&str>, sign_in: &str) -> Mark {
    match text.map(str::trim) {
        None => Mark::Absent,
        Some(t) if is_sign_in(t) && t == sign_in => Mark::ThisSignIn,
        Some(_) => Mark::EarlierSignIn,
    }
}

/// A sign-in as the markers name it: the session's id and its logon time.
fn sign_in_id(session: u32, logon: i64) -> String {
    format!("{session}:{logon}")
}

fn is_sign_in(text: &str) -> bool {
    text.split_once(':').is_some_and(|(s, l)| s.parse::<u32>().is_ok() && l.parse::<i64>().is_ok_and(|l| l > 0))
}

/// This Windows sign-in: its id (`sign_in_id`) and how long ago it began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignIn {
    pub id: String,
    pub age: Duration,
}

fn sign_in() -> Option<SignIn> {
    let s = sys::session()?;
    // FILETIME counts 100 ns.
    let age = Duration::from_nanos(u64::try_from(s.now.saturating_sub(s.logon)).unwrap_or(0).saturating_mul(100));
    Some(SignIn { id: sign_in_id(s.id, s.logon), age })
}

/// The markers in one folder: the machine-local data folder's `keep-running`, or a test's.
struct Markers(PathBuf);

impl Markers {
    fn here() -> Option<Self> {
        crate::settings::local_dir().map(|d| Markers(d.join("keep-running")))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Writes `name` whole, or not at all.
    fn write(&self, name: &str, text: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.0)?;
        let part = self.path(&format!("{name}.part"));
        std::fs::write(&part, text)?;
        std::fs::rename(&part, self.path(name)).inspect_err(|_| {
            let _ = std::fs::remove_file(&part);
        })
    }

    /// What `name` holds; None when it is absent, and empty when it is there but cannot be read.
    fn read(&self, name: &str) -> Option<String> {
        match std::fs::read(self.path(name)) {
            Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => Some(String::new()),
        }
    }

    /// How many seconds before `now` `name` was written (negative: ahead of the clock); None when
    /// it is absent. One whose time cannot be read was written just now.
    fn age(&self, name: &str, now: SystemTime) -> Option<i64> {
        let written = match std::fs::metadata(self.path(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => return Some(0),
            Ok(meta) => match meta.modified() {
                Ok(at) => at,
                Err(_) => return Some(0),
            },
        };
        Some(match now.duration_since(written) {
            Ok(ago) => i64::try_from(ago.as_secs()).unwrap_or(i64::MAX),
            Err(ahead) => -i64::try_from(ahead.duration().as_secs()).unwrap_or(i64::MAX),
        })
    }

    fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.path(name));
    }

    /// A start of SlopBar (this process won the instance lock): the markers it ends, and the sign-in
    /// it started in. Only a start that is not the watchdog's ends a person's Quit.
    fn record_start(&self, sign_in: Option<&str>, by_watchdog: bool) {
        if !by_watchdog {
            self.remove(QUIT);
        }
        self.remove(INSTALLING);
        self.remove(AGENT_QUIT);
        match sign_in {
            Some(id) => {
                let _ = self.write(STARTED, id);
            }
            None => self.remove(STARTED),
        }
    }

    /// A person's Quit, naming this sign-in (an empty marker, which holds, when it cannot be told).
    fn record_quit(&self, sign_in: Option<&str>) {
        let _ = self.write(QUIT, sign_in.unwrap_or_default());
    }

    fn record_agent_quit(&self) {
        let _ = self.write(AGENT_QUIT, &chrono::Utc::now().to_rfc3339());
    }

    fn record_install(&self, version: &str) {
        let _ = self.write(INSTALLING, version);
    }

    /// What the check knows when SlopBar is not running.
    fn facts(&self, now: SystemTime, sign_in: Option<&SignIn>, settings: &Settings, installer: bool) -> Facts {
        let id = sign_in.map_or("", |s| s.id.as_str());
        Facts {
            running: false,
            enabled: settings.keep_running,
            sign_in_age: sign_in.map(|s| s.age),
            quit: quit_mark(self.read(QUIT).as_deref(), id),
            started: started_mark(self.read(STARTED).as_deref(), id),
            launch_at_login: settings.launch_at_login,
            installer,
            installing: self.age(INSTALLING, now),
            agent_quit: self.age(AGENT_QUIT, now),
        }
    }
}

/// Whether `name` is a SlopBar installer's process: the one the updater starts
/// (`SlopBar-<version>-installer.exe`) or a release's (`SlopBar_<version>_x64-setup.exe`).
fn is_installer(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("slopbar") && (name.ends_with("installer.exe") || name.ends_with("setup.exe"))
}

/// Whether a SlopBar installer runs on this machine, in any session (the manual route runs one over
/// ssh). Unknown is no.
fn installer_running() -> bool {
    crate::instance::names().is_some_and(|names| names.iter().any(|n| is_installer(n)))
}

/// The installed executable beside this one (as `app::restart` starts it).
fn app_exe() -> Result<PathBuf, String> {
    Ok(std::env::current_exe().map_err(|e| e.to_string())?.with_file_name(crate::instance::APP_EXE))
}

/// Job limits that would end or bound the app the check starts, were it to stay in the check's job.
#[cfg(windows)]
const TYING: u32 = {
    use windows_sys::Win32::System::JobObjects::*;
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_JOB_TIME
        | JOB_OBJECT_LIMIT_PROCESS_TIME
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
};

/// How to start the app from a process in a job with these limit flags (None: in no job): its
/// creation flags, or Err when it would be tied to the job (ended or bounded with it) and cannot
/// break away. Task Scheduler's jobs have no limits and allow no breakaway: the app stays in one,
/// which is never closed on it.
#[cfg(windows)]
fn spawn_flags(job: Option<u32>) -> Result<u32, ()> {
    use windows_sys::Win32::System::JobObjects::{JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK};
    use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
    match job {
        None => Ok(0),
        Some(f) if f & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK != 0 => Ok(0),
        Some(f) if f & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0 => Ok(CREATE_BREAKAWAY_FROM_JOB),
        Some(f) if f & TYING != 0 => Err(()),
        Some(_) => Ok(0),
    }
}

/// Starts SlopBar on its own: the check's exit code (0 started, 1 it could not be, 2 it would be
/// tied to the check's job).
fn start() -> i32 {
    let Ok(exe) = app_exe() else { return 1 };
    #[cfg(windows)]
    let Ok(flags) = spawn_flags(sys::job_limits()) else { return 2 };
    #[cfg(not(windows))]
    let flags = 0u32;
    let spawn = |flags: u32| {
        let mut command = std::process::Command::new(&exe);
        command
            .arg(STARTED_FLAG)
            .arg(std::process::id().to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // As the Start menu's shortcut starts it.
        if let Some(dir) = exe.parent() {
            command.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(flags);
        }
        #[cfg(not(windows))]
        let _ = flags;
        command.spawn().map(drop)
    };
    // A job that allowed breaking away may stop allowing it; staying in it is then the same start.
    let started = spawn(flags).or_else(|e| if flags == 0 { Err(e) } else { spawn(0) });
    if started.is_ok() { 0 } else { 1 }
}

/// The scheduled task's run: decides, and starts SlopBar if it should. Its exit code is the task's
/// last result: 0 whatever it decided, unless a start failed (`start`).
fn check() -> i32 {
    // At once, before anything else is read: a running SlopBar is left alone.
    if crate::instance::held() {
        return decide_and_start(&Facts { running: true, ..Facts::default() });
    }
    let Some(markers) = Markers::here() else { return 0 };
    let settings = crate::settings::load();
    let sign_in = sign_in();
    decide_and_start(&markers.facts(SystemTime::now(), sign_in.as_ref(), &settings, installer_running()))
}

fn decide_and_start(facts: &Facts) -> i32 {
    match decide(facts) {
        Verdict::Start => start(),
        _ => 0,
    }
}

/// The uninstaller's run: removes this user's task. 0 once it is gone.
fn remove() -> i32 {
    let Some(name) = task_name() else { return 1 };
    if scheduler::delete(&name).is_ok() { 0 } else { 1 }
}

/// The watchdog's own commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Check,
    Remove,
}

/// Which of the watchdog's commands `args` asks for. Only the second argument counts, so a task
/// given more arguments still never starts SlopBar as a second launch, which would open Settings.
fn command(args: &[std::ffi::OsString]) -> Option<Command> {
    let first = args.get(1)?;
    if first == FLAG {
        Some(Command::Check)
    } else if first == REMOVE_FLAG {
        Some(Command::Remove)
    } else {
        None
    }
}

/// main.rs: runs the watchdog's own commands (the check, the uninstaller's removal) and returns
/// their exit code; None for every other start.
pub fn from_args(args: &[std::ffi::OsString]) -> Option<i32> {
    Some(match command(args)? {
        Command::Check => check(),
        Command::Remove => remove(),
    })
}

/// main.rs: the id of the check that started this process (`--watchdog-start <pid>`; 0 when it is
/// missing), so the start claims as a restart does. None for every other start.
pub fn started_by(args: &[std::ffi::OsString]) -> Option<u32> {
    (args.get(1)? == STARTED_FLAG).then(|| args.get(2).and_then(|a| a.to_str()).and_then(|a| a.parse().ok()).unwrap_or(0))
}

static STARTED_BY_WATCHDOG: AtomicBool = AtomicBool::new(false);

/// main.rs, once this process holds the instance lock: records the start (`Markers::record_start`).
pub fn started(by_watchdog: bool) {
    STARTED_BY_WATCHDOG.store(by_watchdog, Ordering::Relaxed);
    if let Some(markers) = Markers::here() {
        markers.record_start(sign_in().map(|s| s.id).as_deref(), by_watchdog);
    }
}

/// A person's Quit from SlopBar's own menu (the tray's or the widget's), before it exits.
pub fn quit_by_person() {
    if let Some(markers) = Markers::here() {
        markers.record_quit(sign_in().map(|s| s.id).as_deref());
    }
}

/// The door's quit_app, which is no person's Quit: the check starts SlopBar again after its grace.
pub fn quit_by_agent() {
    if let Some(markers) = Markers::here() {
        markers.record_agent_quit();
    }
}

/// Right before an update hands over to its installer: the check leaves the installer alone.
pub fn installing(version: &str) {
    if let Some(markers) = Markers::here() {
        markers.record_install(version);
    }
}

// ----------------------------------------------------------------------------------------------
// The task.

/// Whether this build keeps the task: an Ultra release build (whose window app starts no console),
/// running on the installed app's data rather than `SLOPBAR_DATA_DIR`.
fn manages() -> bool {
    builds_task(cfg!(feature = "ultra-updates"), cfg!(debug_assertions)) && !crate::settings::isolated()
}

/// Only Ultra release builds keep the task. A debug build is a console app, which would show a
/// console window at every run.
const fn builds_task(ultra_updates: bool, debug: bool) -> bool {
    ultra_updates && !debug
}

/// This user's task.
fn task_name() -> Option<String> {
    sys::user_sid().map(|sid| format!("{TASK_NAME} {sid}"))
}

/// The task's definition, for `exe`.
pub fn task_xml(exe: &Path) -> String {
    let every = format!("PT{}M", EVERY.as_secs() / 60);
    let description = "Starts SlopBar Ultra again if it stopped, unless it was quit from its menu during this sign-in. \
        SlopBar Ultra keeps this task while its keep_running setting is on, and uninstalling it removes the task.";
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>SlopBar Ultra</Author>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
    <TimeTrigger>
      <Repetition>
        <Interval>{every}</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
      <StartBoundary>{START_BOUNDARY}</StartBoundary>
      <Enabled>true</Enabled>
    </TimeTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{FLAG}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        description = escape(description),
        command = escape(&exe.to_string_lossy()),
    )
}

/// An element of a task's definition: its name (without a prefix), its text and its elements.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Element {
    name: String,
    text: String,
    children: Vec<Element>,
}

impl Element {
    fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }
}

/// The root element of `xml`, read as much of XML as a task's definition uses: a declaration,
/// comments, attributes (skipped, quoted values and all) and the five entities and character
/// references. None when it is not well formed, or uses more (a DTD, CDATA).
fn parse(xml: &str) -> Option<Element> {
    let bytes = xml.as_bytes();
    let mut stack = vec![Element::default()];
    let mut i = 0;
    while i < xml.len() {
        let rest = &xml[i..];
        if rest.starts_with("<?") {
            i += rest.find("?>")? + 2;
        } else if rest.starts_with("<!--") {
            i += rest.find("-->")? + 3;
        } else if rest.starts_with("<!") {
            return None;
        } else if let Some(close) = rest.strip_prefix("</") {
            let end = close.find('>')?;
            let done = stack.pop()?;
            if done.name != local(close[..end].trim()) || stack.is_empty() {
                return None;
            }
            stack.last_mut()?.children.push(done);
            i += 2 + end + 1;
        } else if rest.starts_with('<') {
            let mut j = i + 1;
            let mut quote = None;
            while j < xml.len() {
                match (quote, bytes[j]) {
                    (Some(q), c) if c == q => quote = None,
                    (Some(_), _) => {}
                    (None, c @ (b'"' | b'\'')) => quote = Some(c),
                    (None, b'>') => break,
                    _ => {}
                }
                j += 1;
            }
            if j >= xml.len() {
                return None;
            }
            let tag = &xml[i + 1..j];
            let closed = tag.ends_with('/');
            let name = local(tag.trim_end_matches('/').split(char::is_whitespace).next()?);
            if name.is_empty() {
                return None;
            }
            let element = Element { name: name.into(), ..Element::default() };
            if closed {
                stack.last_mut()?.children.push(element);
            } else {
                stack.push(element);
            }
            i = j + 1;
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            let text = unescape(&rest[..end])?;
            stack.last_mut()?.text.push_str(&text);
            i += end;
        }
    }
    let mut document = stack.pop()?;
    (stack.is_empty() && document.children.len() == 1).then(|| document.children.pop()).flatten()
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let end = rest[at..].find(';')? + at;
        let entity = &rest[at + 1..end];
        out.push(match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = match entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => entity.strip_prefix('#')?.parse().ok()?,
                };
                char::from_u32(code)?
            }
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// A trigger, as far as the check's schedule depends on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: String,
    pub enabled: bool,
    pub interval: Option<String>,
    /// How long it repeats; None for ever.
    pub duration: Option<String>,
    /// It has an end (EndBoundary).
    pub ends: bool,
    /// Its own limit on the task's run.
    pub time_limit: Option<String>,
}

/// What a task does, when and how, with Task Scheduler's defaults filled in where its definition
/// leaves them out (a registered task's XML leaves out most of them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    /// Each action: an Exec's command (quotes trimmed) and arguments, or another kind's name.
    pub actions: Vec<(String, String)>,
    pub principals: usize,
    pub user: Option<String>,
    pub logon: String,
    pub run_level: String,
    pub triggers: Vec<Trigger>,
    pub instances: String,
    pub time_limit: String,
    pub starts_on_batteries: bool,
    pub stops_on_batteries: bool,
    pub enabled: bool,
    pub only_if_idle: bool,
    pub only_if_network: bool,
}

/// `xml`'s shape; None when it is no task's definition.
pub fn shape(xml: &str) -> Option<Shape> {
    let task = parse(xml)?;
    if task.name != "Task" {
        return None;
    }
    let settings = task.child("Settings");
    let setting = |name: &str| settings.and_then(|s| s.text_of(name));
    let flag = |name: &str, default: bool| setting(name).map_or(default, |t| t == "true" || t == "1");
    let principals: Vec<&Element> = task.child("Principals").map(|p| p.children.iter().filter(|c| c.name == "Principal").collect()).unwrap_or_default();
    let principal = principals.first();
    let actions = task.child("Actions").map_or_else(Vec::new, |a| {
        a.children
            .iter()
            .map(|action| match action.name.as_str() {
                "Exec" => (
                    action.text_of("Command").unwrap_or_default().trim_matches('"').to_string(),
                    action.text_of("Arguments").unwrap_or_default().to_string(),
                ),
                other => (other.to_string(), String::new()),
            })
            .collect()
    });
    let triggers = task.child("Triggers").map_or_else(Vec::new, |t| {
        t.children
            .iter()
            .map(|trigger| {
                let repetition = trigger.child("Repetition");
                Trigger {
                    kind: trigger.name.clone(),
                    enabled: trigger.text_of("Enabled").is_none_or(|t| t == "true" || t == "1"),
                    interval: repetition.and_then(|r| r.text_of("Interval")).map(str::to_string),
                    duration: repetition.and_then(|r| r.text_of("Duration")).filter(|d| !d.is_empty()).map(str::to_string),
                    ends: trigger.child("EndBoundary").is_some(),
                    time_limit: trigger.text_of("ExecutionTimeLimit").map(str::to_string),
                }
            })
            .collect()
    });
    Some(Shape {
        actions,
        principals: principals.len(),
        user: principal.and_then(|p| p.text_of("UserId")).map(str::to_string),
        logon: principal.and_then(|p| p.text_of("LogonType")).unwrap_or_default().to_string(),
        run_level: principal.and_then(|p| p.text_of("RunLevel")).unwrap_or("LeastPrivilege").to_string(),
        triggers,
        instances: setting("MultipleInstancesPolicy").unwrap_or("IgnoreNew").to_string(),
        time_limit: setting("ExecutionTimeLimit").unwrap_or("PT72H").to_string(),
        starts_on_batteries: !flag("DisallowStartIfOnBatteries", true),
        stops_on_batteries: flag("StopIfGoingOnBatteries", true),
        enabled: flag("Enabled", true),
        only_if_idle: flag("RunOnlyIfIdle", false),
        only_if_network: flag("RunOnlyIfNetworkAvailable", false),
    })
}

/// How `found` differs from `expected`, in words; empty when the task is as it should be. `user`
/// is this user's SID: a task that runs as anyone else is not this user's.
pub fn problems(found: &Shape, expected: &Shape, user: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let same_action = |a: &(String, String), b: &(String, String)| a.0.to_lowercase() == b.0.to_lowercase() && a.1 == b.1;
    let actions_match = found.actions.len() == expected.actions.len() && found.actions.iter().zip(&expected.actions).all(|(a, b)| same_action(a, b));
    if !actions_match {
        let said: Vec<String> = found.actions.iter().map(|(c, a)| format!("{} {a}", file_name(c)).trim().to_string()).collect();
        out.push(format!("runs {}, not this app's {FLAG}", if said.is_empty() { "nothing".to_string() } else { said.join(" and ") }));
    }
    if found.principals != 1 {
        out.push(format!("{} principals", found.principals));
    }
    // Task Scheduler keeps the SID of the user who registered it; a name instead says nothing either way.
    if let (Some(found_user), Some(user)) = (found.user.as_deref(), user) {
        if found_user.to_ascii_uppercase().starts_with("S-1-") && !found_user.eq_ignore_ascii_case(user) {
            out.push("runs as another user".into());
        }
    }
    if found.logon != expected.logon {
        out.push(format!("logon type {}, not {}", or_none(&found.logon), expected.logon));
    }
    if found.run_level != expected.run_level {
        out.push(format!("run level {}, not {}", found.run_level, expected.run_level));
    }
    if found.triggers != expected.triggers {
        let said: Vec<String> = found
            .triggers
            .iter()
            .map(|t| {
                let mut words = vec![t.kind.clone()];
                if !t.enabled {
                    words.push("disabled".into());
                }
                words.push(t.interval.as_ref().map_or("not repeating".into(), |i| format!("every {i}")));
                if let Some(d) = &t.duration {
                    words.push(format!("for {d}"));
                }
                if t.ends {
                    words.push("with an end".into());
                }
                if let Some(l) = &t.time_limit {
                    words.push(format!("limit {l}"));
                }
                words.join(" ")
            })
            .collect();
        out.push(format!("triggers: {}", if said.is_empty() { "none".to_string() } else { said.join("; ") }));
    }
    if found.instances != expected.instances {
        out.push(format!("multiple instances: {}, not {}", found.instances, expected.instances));
    }
    if found.time_limit != expected.time_limit {
        out.push(format!("execution time limit {}, not none", found.time_limit));
    }
    if found.starts_on_batteries != expected.starts_on_batteries || found.stops_on_batteries != expected.stops_on_batteries {
        out.push("held back or stopped on batteries".into());
    }
    if found.enabled != expected.enabled {
        out.push("disabled".into());
    }
    if found.only_if_idle != expected.only_if_idle || found.only_if_network != expected.only_if_network {
        out.push("runs only when idle or on a network".into());
    }
    out
}

fn or_none(text: &str) -> &str {
    if text.is_empty() { "none" } else { text }
}

fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// The task as it is registered, against the one this app would register.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Judged {
    Ok,
    Missing,
    Wrong(Vec<String>),
}

fn judge(found: Option<&str>, expected: &Shape, user: Option<&str>) -> Judged {
    let Some(xml) = found else { return Judged::Missing };
    match shape(xml) {
        None => Judged::Wrong(vec!["its definition cannot be read".into()]),
        Some(found) => {
            let problems = problems(&found, expected, user);
            if problems.is_empty() { Judged::Ok } else { Judged::Wrong(problems) }
        }
    }
}

/// The PE subsystem of the executable that `head` (its first bytes) begins: 2 is a window app,
/// 3 a console app.
fn subsystem(head: &[u8]) -> Option<u16> {
    let word = |at: usize| head.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    if head.get(..2)? != b"MZ" {
        return None;
    }
    let pe = u32::from_le_bytes(head.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if head.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let optional = pe + 4 + 20;
    if !matches!(word(optional)?, 0x10b | 0x20b) {
        return None;
    }
    word(optional + 68)
}

const WINDOW_APP: u16 = 2;

/// Whether `exe` is a window app, which starts without a console window.
fn is_window_app(exe: &Path) -> bool {
    use std::io::Read;
    let mut head = vec![0; 4096];
    let read = std::fs::File::open(exe).and_then(|mut f| f.read(&mut head));
    read.is_ok_and(|n| subsystem(&head[..n]) == Some(WINDOW_APP))
}

/// An OLE date (days since 1899-12-30, local time) as UTC; None before 1900, which Task Scheduler
/// gives for a task that has not run.
fn ole_date(days: f64) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    if !(days >= 1.0 && days < 2_958_466.0) {
        return None;
    }
    let base = chrono::NaiveDate::from_ymd_opt(1899, 12, 30)?.and_hms_opt(0, 0, 0)?;
    let local = base + chrono::Duration::milliseconds((days * 86_400_000.0).round() as i64);
    chrono::Local.from_local_datetime(&local).earliest().map(|t| t.with_timezone(&chrono::Utc))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The value of `keep_running` last applied, so a settings save that leaves it alone does nothing.
static APPLIED: Mutex<Option<bool>> = Mutex::new(None);
/// One change of the task at a time.
static SYNC: Mutex<()> = Mutex::new(());
/// Why the last change of the task failed.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// app.rs (`apply_settings`, at startup and after every settings change): makes the task match
/// `keep_running`, on a thread of its own, when this build keeps it (`manages`) and the value
/// changed since it was last applied.
pub fn apply(enabled: bool) {
    if !manages() {
        return;
    }
    {
        let mut applied = lock(&APPLIED);
        if *applied == Some(enabled) {
            return;
        }
        *applied = Some(enabled);
    }
    let _ = std::thread::Builder::new().name("keep-running".into()).spawn(|| {
        let _one = lock(&SYNC);
        // The latest value asked for, should another change have come meanwhile.
        let enabled = lock(&APPLIED).unwrap_or(true);
        *lock(&LAST_ERROR) = sync(enabled).err();
    });
}

/// Creates the task, or repairs it when it differs from this app's (another executable, a changed
/// schedule, disabled...), or removes it while `enabled` is off.
fn sync(enabled: bool) -> Result<(), String> {
    let name = task_name().ok_or("this user's SID could not be read")?;
    if !enabled {
        return scheduler::delete(&name).map(drop);
    }
    let exe = app_exe()?;
    // Never a console app: it would show a console window at every run.
    if !is_window_app(&exe) {
        return Err(format!("{} is not a window app", file_name(&exe.to_string_lossy())));
    }
    let xml = task_xml(&exe);
    let expected = shape(&xml).ok_or("this app's own task definition cannot be read")?;
    let found = scheduler::read(&name)?;
    match judge(found.as_ref().map(|t| t.xml.as_str()), &expected, sys::user_sid().as_deref()) {
        Judged::Ok => Ok(()),
        _ => scheduler::register(&name, &xml),
    }
}

/// get_app_info's `keep_running`: the setting, whether this build keeps the task, the task as it is
/// (ok, missing, wrong with its problems, or unknown with the error) and its last run.
pub fn report(enabled: bool) -> serde_json::Value {
    let managed = manages();
    let started_by_watchdog = STARTED_BY_WATCHDOG.load(Ordering::Relaxed);
    let last_error = lock(&LAST_ERROR).clone();
    let read = task_name().ok_or_else(|| "this user's SID could not be read".to_string()).and_then(|name| scheduler::read(&name));
    let expected = app_exe().ok().and_then(|exe| shape(&task_xml(&exe)));
    let (task, problems, error, last_run, last_result) = match (read, expected) {
        (Err(e), _) => ("unknown", vec![], Some(e), None, None),
        (Ok(_), None) => ("unknown", vec![], Some("this app's own task definition cannot be read".to_string()), None, None),
        (Ok(found), Some(expected)) => {
            let (run, result) = found.as_ref().map_or((None, None), |t| (ole_date(t.last_run), Some(t.last_result)));
            match judge(found.as_ref().map(|t| t.xml.as_str()), &expected, sys::user_sid().as_deref()) {
                Judged::Ok => ("ok", vec![], last_error, run, result),
                Judged::Missing => ("missing", vec![], last_error, None, None),
                Judged::Wrong(p) => ("wrong", p, last_error, run, result),
            }
        }
    };
    serde_json::json!({
        "enabled": enabled,
        "managed": managed,
        "task": task,
        "problems": problems,
        "error": error,
        "last_run": last_run.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "last_result": last_result,
        "started_by_watchdog": started_by_watchdog,
    })
}

// ----------------------------------------------------------------------------------------------
// Windows.

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};

    /// This process's Windows session: its id, when its user signed in and now (FILETIME).
    pub struct Session {
        pub id: u32,
        pub logon: i64,
        pub now: i64,
    }

    pub fn session() -> Option<Session> {
        use windows_sys::Win32::System::RemoteDesktop::{
            WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfo, WTSINFOW, WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION,
        };
        let mut buffer: *mut u16 = std::ptr::null_mut();
        let mut bytes = 0u32;
        // SAFETY: out-parameters only; the buffer the call allocates is read while it is at least a
        // WTSINFOW long, then freed once.
        unsafe {
            if WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSSessionInfo, &mut buffer, &mut bytes) == 0 || buffer.is_null() {
                return None;
            }
            let info = (bytes as usize >= std::mem::size_of::<WTSINFOW>()).then(|| std::ptr::read_unaligned(buffer as *const WTSINFOW));
            WTSFreeMemory(buffer.cast());
            let info = info?;
            (info.LogonTime > 0).then_some(Session { id: info.SessionId, logon: info.LogonTime, now: info.CurrentTime })
        }
    }

    /// This process's user, as a SID string ("S-1-5-21-...").
    pub fn user_sid() -> Option<String> {
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: a standard token query into a u64-aligned buffer the system sized; the token and
        // the string the conversion allocates are released once.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buffer = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let read = GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), len, &mut len);
            CloseHandle(token);
            if read == 0 {
                return None;
            }
            let sid = (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid;
            let mut text = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut text) == 0 {
                return None;
            }
            let n = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
            LocalFree(text.cast());
            Some(sid)
        }
    }

    /// The limit flags of the job this process is in; None when it is in none, or that cannot be
    /// told.
    pub fn job_limits() -> Option<u32> {
        use windows_sys::Win32::System::JobObjects::{IsProcessInJob, JobObjectExtendedLimitInformation, QueryInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: out-parameters only; a null job handle asks about this process's own job.
        unsafe {
            let mut in_job = 0;
            if IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) == 0 || in_job == 0 {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            let size = std::mem::size_of_val(&info) as u32;
            if QueryInformationJobObject(std::ptr::null_mut(), JobObjectExtendedLimitInformation, (&mut info as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size, std::ptr::null_mut()) == 0 {
                // In a job whose limits cannot be read: as one that ties, unless it lets go.
                return Some(super::TYING);
            }
            Some(info.BasicLimitInformation.LimitFlags)
        }
    }
}

#[cfg(not(windows))]
mod sys {
    pub struct Session {
        pub id: u32,
        pub logon: i64,
        pub now: i64,
    }
    pub fn session() -> Option<Session> {
        None
    }
    pub fn user_sid() -> Option<String> {
        None
    }
}

/// The task in Task Scheduler, through its COM interface: no console program to start (schtasks
/// would print the definition in the console's code page), and the definition read back exactly.
/// Each call runs on a thread of its own with COM started there, and is waited for at most
/// SCHEDULER_LIMIT, so a stuck Task Scheduler never holds the door or the uninstaller.
#[cfg(windows)]
mod scheduler {
    use super::SCHEDULER_LIMIT;
    use windows::core::BSTR;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
    use windows::Win32::System::TaskScheduler::{ITaskFolder, ITaskService, TaskScheduler, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN};
    use windows::Win32::System::Variant::VARIANT;

    /// A registered task: its definition and its last run (an OLE date, and its result).
    pub struct Registered {
        pub xml: String,
        pub last_run: f64,
        pub last_result: i32,
    }

    /// HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND): no such task.
    const NOT_FOUND: i32 = 0x8007_0002_u32 as i32;

    fn describe(e: &windows::core::Error) -> String {
        format!("Task Scheduler: {:#010x} {}", e.code().0, e.message().trim())
    }

    fn on_com_thread<T: Send + 'static>(work: impl FnOnce(&ITaskFolder) -> windows::core::Result<T> + Send + 'static) -> Result<T, String> {
        let (done, answer) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("keep-running-scheduler".into())
            .spawn(move || {
                // SAFETY: COM is started on this new thread and ended on it once every object it
                // made (the folder, in `run`) has been released.
                let result = unsafe {
                    let started = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
                    let run = || -> windows::core::Result<T> {
                        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
                        let none = VARIANT::default();
                        service.Connect(&none, &none, &none, &none)?;
                        let folder = service.GetFolder(&BSTR::from("\\"))?;
                        work(&folder)
                    };
                    let result = run().map_err(|e| describe(&e));
                    if started {
                        CoUninitialize();
                    }
                    result
                };
                let _ = done.send(result);
            })
            .map_err(|e| e.to_string())?;
        answer.recv_timeout(SCHEDULER_LIMIT).map_err(|_| format!("Task Scheduler did not answer within {} s", SCHEDULER_LIMIT.as_secs()))?
    }

    /// The task named `name`; None when there is none.
    pub fn read(name: &str) -> Result<Option<Registered>, String> {
        let name = name.to_string();
        on_com_thread(move |folder| {
            // SAFETY: COM calls on an interface this thread holds.
            unsafe {
                match folder.GetTask(&BSTR::from(name.as_str())) {
                    Err(e) if e.code().0 == NOT_FOUND => Ok(None),
                    Err(e) => Err(e),
                    Ok(task) => Ok(Some(Registered { xml: task.Xml()?.to_string(), last_run: task.LastRunTime()?, last_result: task.LastTaskResult()? })),
                }
            }
        })
    }

    /// Creates the task named `name`, or replaces it, as `xml` defines it, for this user while
    /// signed in (interactive token: no password is stored).
    pub fn register(name: &str, xml: &str) -> Result<(), String> {
        let (name, xml) = (name.to_string(), xml.to_string());
        on_com_thread(move |folder| {
            let none = VARIANT::default();
            let (name, xml) = (BSTR::from(name.as_str()), BSTR::from(xml.as_str()));
            // SAFETY: COM calls on an interface this thread holds.
            unsafe { folder.RegisterTask(&name, &xml, TASK_CREATE_OR_UPDATE.0, &none, &none, TASK_LOGON_INTERACTIVE_TOKEN, &none).map(drop) }
        })
    }

    /// Removes the task named `name`: whether there was one.
    pub fn delete(name: &str) -> Result<bool, String> {
        let name = name.to_string();
        on_com_thread(move |folder| {
            // SAFETY: COM calls on an interface this thread holds.
            unsafe {
                match folder.DeleteTask(&BSTR::from(name.as_str()), 0) {
                    Ok(()) => Ok(true),
                    Err(e) if e.code().0 == NOT_FOUND => Ok(false),
                    Err(e) => Err(e),
                }
            }
        })
    }
}

#[cfg(not(windows))]
mod scheduler {
    pub struct Registered {
        pub xml: String,
        pub last_run: f64,
        pub last_result: i32,
    }
    const UNSUPPORTED: &str = "the watchdog's task is Windows only";
    pub fn read(_name: &str) -> Result<Option<Registered>, String> {
        Err(UNSUPPORTED.into())
    }
    pub fn register(_name: &str, _xml: &str) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn delete(_name: &str) -> Result<bool, String> {
        Err(UNSUPPORTED.into())
    }
}

#[cfg(test)]
mod tests;
