//! SlopBar Ultra's watchdog (fork-only; docs/ULTRA-RELEASES.md, "Keeping SlopBar Ultra running").
//!
//! A scheduled task of the signed-in user's own runs `slopbar.exe --keep-running` every 5 minutes,
//! only while that user is signed in. The check starts SlopBar Ultra again when it has stopped for
//! any reason but a person's Quit during this Windows sign-in: a crash, a kill, an agent's quit_app
//! (once its grace has passed: the manual install route quits through the door, installs and starts
//! SlopBar itself), a restart or an update that failed. It never races an installer or an uninstall,
//! never starts SlopBar while Windows signs out or shuts down, and never signals the running app: a
//! plain second launch would open Settings there (`instance::claim` hands over), so the check only
//! looks at the instance lock (`instance::held`) and ends at once when it is held.
//!
//! The check starts SlopBar the way the shell does, through Explorer (`sys::shell_execute`): the app
//! is then Explorer's child, at normal CPU, I/O and memory priority, and in none of Task Scheduler's
//! jobs. Only where the shell cannot be reached does it start the app itself, at normal priority,
//! breaking away from the task's job when that job allows it, and never into a job that would end or
//! bound the app (`launch_with`). Either way it passes `--watchdog-start <nonce>`, the nonce it wrote
//! beside the markers just before: the start that finds it claims the instance lock quietly, as a
//! restart does, and no other start counts as the watchdog's (`start_kind`).
//!
//! Ultra release builds create or repair the task at startup and remove it while `keep_running` is
//! off (`apply`), retrying a failure a few times; a task a person disabled is left disabled. The
//! uninstaller holds every check, removes the task without running any slopbar.exe, and the next
//! install lets go (src-tauri/nsis/ultra-hooks.nsh).
//!
//! What the check reads, in the machine-local data folder's `keep-running`:
//! - `quit`: a person's Quit (the tray's menu or the widget's), naming the Windows sign-in it was
//!   chosen in, so it lapses by itself at the next sign-in. Every start but the watchdog's removes it.
//! - `started`: the sign-in SlopBar last started in, which tells a stop from a sign-in it never ran in.
//! - `installing`: written right before an update hands over to its installer; honoured INSTALL_GRACE.
//! - `agent-quit`: the door's quit_app; honoured AGENT_QUIT_GRACE.
//! - `uninstalling`: written by the uninstaller; honoured UNINSTALL_GRACE, and removed by the
//!   installer.
//! - `watchdog-start`: the nonce of the start the check just made.
//! - `last-check`: what the check last decided and its exit code (none while SlopBar runs).
//!
//! A sign-in is the Windows session's id and the time its user signed in (WTSINFO): every process of
//! the sign-in reads the same pair, elevated or not (a token's logon id differs between an admin's two
//! linked tokens), locking and unlocking or reconnecting keeps it, and the next sign-in gets a new
//! logon time even where Windows reuses the session id.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime};

use crate::settings::Settings;

/// The task's argument: the check.
pub const FLAG: &str = "--keep-running";
/// What the check starts SlopBar with, followed by the nonce it wrote (`Markers::write_handshake`).
pub const STARTED_FLAG: &str = "--watchdog-start";
/// The task's name, followed by the user's SID: task names are the machine's, the task is the user's.
pub const TASK_NAME: &str = "SlopBar Ultra keep running";
/// The startup entry's name in the Run key: tauri-plugin-autostart names it after the app (the
/// product name in tauri.conf.json).
pub const RUN_NAME: &str = "SlopBar";
/// How often the task runs the check.
pub const EVERY: Duration = Duration::from_secs(5 * 60);
/// The task's priority: normal CPU, I/O and memory priority. Task Scheduler's default, 7, runs the
/// check, and anything it starts, below normal, at low I/O and low memory priority.
pub const PRIORITY: u8 = 5;
/// How long an update's install marker holds the check off. The handover runs the exit cleanup
/// (within HANDOFF_LIMIT) and the installer then takes well under a minute; an install that never
/// started SlopBar again has failed by then, and the next run starts it.
pub const INSTALL_GRACE: Duration = Duration::from_secs(10 * 60);
/// How long the door's quit_app holds the check off: the manual install route quits through the
/// door, installs and starts SlopBar itself within it (docs/ULTRA-RELEASES.md).
pub const AGENT_QUIT_GRACE: Duration = Duration::from_secs(15 * 60);
/// How long a new sign-in is left to the startup entry before the check starts SlopBar itself.
pub const SIGN_IN_GRACE: Duration = Duration::from_secs(5 * 60);
/// How long the uninstaller's marker holds the check and the task's registration. An uninstall
/// takes seconds; one that failed lets go by itself, and the next install lets go at once.
pub const UNINSTALL_GRACE: Duration = Duration::from_secs(60 * 60);
/// How long the nonce of a start the check made counts: the app reads it within seconds.
pub const HANDSHAKE_GRACE: Duration = Duration::from_secs(2 * 60);
/// A start with no arguments, from Explorer and no shortcut, this soon after the sign-in, is the
/// startup entry's (`sign_in_start`): Explorer runs the entries once, as it starts.
pub const SIGN_IN_START_WINDOW: Duration = Duration::from_secs(30 * 60);
/// How long Task Scheduler gets to answer a call: the door's get_app_info and the tray's Quit wait.
pub const SCHEDULER_LIMIT: Duration = Duration::from_secs(10);
/// How long a failed registration waits before each retry: three more tries, then none until the
/// next start or settings change.
pub const RETRIES: [Duration; 3] = [Duration::from_secs(60), Duration::from_secs(5 * 60), Duration::from_secs(15 * 60)];
/// Where the task's time trigger starts repeating from; any time in the past does.
pub const START_BOUNDARY: &str = "2026-01-01T00:00:00";
/// A marker this far ahead of the clock still counts as just written (a clock set back a little).
const CLOCK_SLACK_SECONDS: i64 = 60;
/// In every build that handles `--keep-running`: restart_app looks for it in the executable it
/// restarts into (`handles_watchdog`), and removes the task before starting one without it.
pub const SENTINEL: &str = "slopbar-ultra:keep-running:handles:v1";

const QUIT: &str = "quit";
const STARTED: &str = "started";
const INSTALLING: &str = "installing";
const AGENT_QUIT: &str = "agent-quit";
const UNINSTALLING: &str = "uninstalling";
const HANDSHAKE: &str = "watchdog-start";
const LAST_CHECK: &str = "last-check";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ----------------------------------------------------------------------------------------------
// The decision.

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
    /// Windows is signing out or shutting down.
    pub shutting_down: bool,
    /// The `keep_running` setting.
    pub enabled: bool,
    /// How many seconds ago the uninstaller's marker was written.
    pub uninstalling: Option<i64>,
    /// How long ago this Windows sign-in began; None when the sign-in cannot be told.
    pub sign_in_age: Option<Duration>,
    /// A person's Quit.
    pub quit: Mark,
    /// SlopBar's last start.
    pub started: Mark,
    /// Windows starts SlopBar at sign-in: the setting is on and the startup entry starts this app,
    /// neither disabled in Task Manager nor blocked by policy (`launches_at_login`).
    pub launches_at_login: bool,
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
    ShuttingDown,
    Off,
    Uninstalling,
    UnknownSignIn,
    QuitByPerson,
    Installing,
    QuitByAgent,
    /// SlopBar has not run in this sign-in and Windows does not start it at sign-in: nothing stopped.
    NeverStarted,
    /// A new sign-in, which the startup entry starts SlopBar in.
    SignInStarting,
}

impl Verdict {
    pub fn name(self) -> &'static str {
        match self {
            Verdict::Start => "start",
            Verdict::Running => "running",
            Verdict::ShuttingDown => "shutting_down",
            Verdict::Off => "off",
            Verdict::Uninstalling => "uninstalling",
            Verdict::UnknownSignIn => "unknown_sign_in",
            Verdict::QuitByPerson => "quit_by_person",
            Verdict::Installing => "installing",
            Verdict::QuitByAgent => "quit_by_agent",
            Verdict::NeverStarted => "never_started",
            Verdict::SignInStarting => "sign_in_starting",
        }
    }
}

/// The check's choice. A running SlopBar comes first, so the check ends at once on it.
pub fn decide(f: &Facts) -> Verdict {
    if f.running {
        return Verdict::Running;
    }
    if f.shutting_down {
        return Verdict::ShuttingDown;
    }
    if !f.enabled {
        return Verdict::Off;
    }
    if f.uninstalling.is_some_and(|a| fresh(a, UNINSTALL_GRACE)) {
        return Verdict::Uninstalling;
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
        if !f.launches_at_login {
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

/// The sign-in of the session `session`, whose user signed in at `logon` (a FILETIME, in 100 ns),
/// seen at `now`; None when it has no logon time.
pub fn sign_in_from(session: u32, logon: i64, now: i64) -> Option<SignIn> {
    if logon <= 0 {
        return None;
    }
    let ticks = u64::try_from(now.saturating_sub(logon)).unwrap_or(0);
    Some(SignIn { id: sign_in_id(session, logon), age: Duration::from_nanos(ticks.saturating_mul(100)) })
}

// ----------------------------------------------------------------------------------------------
// The startup entry.

/// What Windows does with SlopBar's startup entry (HKCU Run) at sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEntry {
    /// It starts this app.
    Ok,
    Missing,
    /// It starts another program (another folder's slopbar.exe, or none that exists).
    Elsewhere,
    /// Turned off in Task Manager (StartupApproved).
    Disabled,
    /// A policy keeps Windows from running the user's startup entries (DisableCurrentUserRun).
    Blocked,
}

impl RunEntry {
    pub fn name(self) -> &'static str {
        match self {
            RunEntry::Ok => "ok",
            RunEntry::Missing => "missing",
            RunEntry::Elsewhere => "elsewhere",
            RunEntry::Disabled => "disabled",
            RunEntry::Blocked => "blocked",
        }
    }
}

/// The program a Run entry's command starts: the quoted path, or the text up to the first `.exe`.
pub fn run_program(command: &str) -> Option<PathBuf> {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        return rest.split('"').next().filter(|p| !p.is_empty()).map(PathBuf::from);
    }
    let end = command.to_ascii_lowercase().find(".exe")? + ".exe".len();
    Some(PathBuf::from(&command[..end]))
}

/// The startup entry as Windows will treat it: `command` (the Run entry's value, if any),
/// `approved` (its StartupApproved value: an odd first byte is Task Manager's "disabled"), whether a
/// policy blocks the user's startup entries, and whether a program is this app.
pub fn run_entry_from(command: Option<&str>, approved: Option<&[u8]>, blocked: bool, is_this_app: impl Fn(&Path) -> bool) -> RunEntry {
    if blocked {
        return RunEntry::Blocked;
    }
    let Some(command) = command else { return RunEntry::Missing };
    if !run_program(command).is_some_and(|p| is_this_app(&p)) {
        return RunEntry::Elsewhere;
    }
    if approved.and_then(|b| b.first()).is_some_and(|first| first & 1 == 1) {
        return RunEntry::Disabled;
    }
    RunEntry::Ok
}

/// Whether Windows starts SlopBar at sign-in: the setting asks for it and the entry does it.
pub fn launches_at_login(setting: bool, entry: RunEntry) -> bool {
    setting && entry == RunEntry::Ok
}

/// Whether `a` and `b` are the same file (paths may differ in case, or be 8.3 short names).
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
        _ => false,
    }
}

// ----------------------------------------------------------------------------------------------
// The markers.

/// The record of what the check last decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastCheck {
    pub at: SystemTime,
    pub decision: String,
    pub exit: i32,
}

/// The markers in one folder: the machine-local data folder's `keep-running`, or a test's.
pub struct Markers(PathBuf);

impl Markers {
    pub fn here() -> Option<Self> {
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

    /// When `name` was written; None when it is absent, and `now` when its time cannot be read.
    fn written(&self, name: &str, now: SystemTime) -> Option<SystemTime> {
        match std::fs::metadata(self.path(name)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => Some(now),
            Ok(meta) => Some(meta.modified().unwrap_or(now)),
        }
    }

    /// How many seconds before `now` `name` was written (negative: ahead of the clock); None when
    /// it is absent. One whose time cannot be read was written just now.
    fn age(&self, name: &str, now: SystemTime) -> Option<i64> {
        let written = self.written(name, now)?;
        Some(match now.duration_since(written) {
            Ok(ago) => i64::try_from(ago.as_secs()).unwrap_or(i64::MAX),
            Err(ahead) => -i64::try_from(ahead.duration().as_secs()).unwrap_or(i64::MAX),
        })
    }

    fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.path(name));
    }

    /// A start of SlopBar (this process won the instance lock): the markers it ends, and the sign-in
    /// it started in. Only a start that is not the watchdog's ends a person's Quit; no start ends an
    /// uninstall's hold (the installer does).
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
    fn record_quit(&self, sign_in: Option<&str>) -> std::io::Result<()> {
        self.write(QUIT, sign_in.unwrap_or_default())
    }

    fn record_agent_quit(&self) {
        let _ = self.write(AGENT_QUIT, &chrono::Utc::now().to_rfc3339());
    }

    fn record_install(&self, version: &str) {
        let _ = self.write(INSTALLING, version);
    }

    /// What the check decided, and its exit code.
    fn record_check(&self, verdict: Verdict, exit: i32) {
        let _ = self.write(LAST_CHECK, &format!("{} {exit}", verdict.name()));
    }

    pub fn last_check(&self, now: SystemTime) -> Option<LastCheck> {
        let text = self.read(LAST_CHECK)?;
        let (decision, exit) = text.trim().split_once(' ')?;
        Some(LastCheck { at: self.written(LAST_CHECK, now)?, decision: decision.to_string(), exit: exit.parse().ok()? })
    }

    /// The nonce of the start the check is about to make.
    fn write_handshake(&self, nonce: &str) -> std::io::Result<()> {
        self.write(HANDSHAKE, nonce)
    }

    /// Whether `nonce` is the one the check wrote, recently: the start that presents it is the
    /// watchdog's. It counts once.
    fn take_handshake(&self, nonce: &str, now: SystemTime) -> bool {
        let matches = !nonce.is_empty() && self.read(HANDSHAKE).is_some_and(|t| t.trim() == nonce);
        let recent = self.age(HANDSHAKE, now).is_some_and(|a| fresh(a, HANDSHAKE_GRACE));
        if matches {
            self.remove(HANDSHAKE);
        }
        matches && recent
    }

    /// Whether an uninstall holds the task now.
    fn uninstalling(&self, now: SystemTime) -> bool {
        self.age(UNINSTALLING, now).is_some_and(|a| fresh(a, UNINSTALL_GRACE))
    }
}

// ----------------------------------------------------------------------------------------------
// The check.

/// What the check reads from the machine, apart from the markers.
pub trait Machine {
    /// Another process holds SlopBar's instance lock.
    fn running(&self) -> bool;
    fn shutting_down(&self) -> bool;
    fn settings(&self) -> Settings;
    fn sign_in(&self) -> Option<SignIn>;
    fn installer_running(&self) -> bool;
    fn run_entry(&self, app: &Path) -> RunEntry;
    /// The installed executable beside this one (as `app::restart` starts it).
    fn app_exe(&self) -> Result<PathBuf, String>;
    fn now(&self) -> SystemTime;
}

/// This computer.
pub struct Windows;

impl Machine for Windows {
    fn running(&self) -> bool {
        crate::instance::held()
    }
    fn shutting_down(&self) -> bool {
        sys::shutting_down()
    }
    fn settings(&self) -> Settings {
        crate::settings::load()
    }
    fn sign_in(&self) -> Option<SignIn> {
        sys::session().and_then(|s| sign_in_from(s.id, s.logon, s.now))
    }
    fn installer_running(&self) -> bool {
        crate::instance::names().is_some_and(|names| names.iter().any(|n| is_installer(n)))
    }
    fn run_entry(&self, app: &Path) -> RunEntry {
        sys::run_entry(app)
    }
    fn app_exe(&self) -> Result<PathBuf, String> {
        Ok(beside(&std::env::current_exe().map_err(|e| e.to_string())?))
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// The installed slopbar.exe beside `exe`, even if `exe` was renamed aside for an update.
fn beside(exe: &Path) -> PathBuf {
    exe.with_file_name(crate::instance::APP_EXE)
}

/// Whether `name` is a SlopBar installer's process: the one the updater starts
/// (`SlopBar-<version>-installer.exe`) or a release's (`SlopBar_<version>_x64-setup.exe`).
fn is_installer(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("slopbar") && (name.ends_with("installer.exe") || name.ends_with("setup.exe"))
}

/// How the app was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launched {
    /// By Explorer, as the shell starts programs.
    Shell,
    /// By the check itself, the shell being out of reach.
    Spawned,
}

/// Why the app was not started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    Failed(String),
    /// It would have been tied to a job that ends or bounds it.
    Tied,
}

/// Starts the app.
pub trait Launcher {
    fn launch(&self, exe: &Path, args: &[String], dir: &Path) -> Result<Launched, Refused>;
}

/// Through the shell, else by itself (`launch_with`).
pub struct ShellLauncher;

impl Launcher for ShellLauncher {
    fn launch(&self, exe: &Path, args: &[String], dir: &Path) -> Result<Launched, Refused> {
        launch_with(|| sys::shell_execute(exe, &args.join(" "), dir), |flags| spawn(exe, args, dir, flags), sys::job_limits())
    }
}

fn spawn(exe: &Path, args: &[String], dir: &Path, flags: u32) -> std::io::Result<()> {
    let mut command = std::process::Command::new(exe);
    command.args(args).current_dir(dir).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(flags);
    }
    #[cfg(not(windows))]
    let _ = flags;
    command.spawn().map(drop)
}

/// Normal priority, whatever the check runs at.
const NORMAL_PRIORITY_CLASS: u32 = 0x0000_0020;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const JOB_OBJECT_LIMIT_BREAKAWAY_OK: u32 = 0x0000_0800;
const JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK: u32 = 0x0000_1000;
/// Job limits that would end or bound the app, were it to stay in the check's job: kill on close,
/// job and process time, job and process memory, and the number of processes.
const TYING: u32 = 0x0000_2000 | 0x0000_0004 | 0x0000_0002 | 0x0000_0200 | 0x0000_0100 | 0x0000_0008;

/// Starts the app through the shell; where that fails, by itself at normal priority, breaking away
/// from the check's job (`job`, its limit flags; None outside a job) where the job allows it, and
/// staying in one only when that job can neither end nor bound it.
pub fn launch_with(shell: impl FnOnce() -> Result<(), String>, spawn: impl Fn(u32) -> std::io::Result<()>, job: Option<u32>) -> Result<Launched, Refused> {
    if shell().is_ok() {
        return Ok(Launched::Shell);
    }
    let flags = spawn_flags(job).map_err(|()| Refused::Tied)?;
    spawn(flags | NORMAL_PRIORITY_CLASS)
        // A job that allowed breaking away may stop allowing it; staying in it is then the same start.
        .or_else(|e| if flags & CREATE_BREAKAWAY_FROM_JOB != 0 && job.is_some_and(|f| f & TYING == 0) { spawn(NORMAL_PRIORITY_CLASS) } else { Err(e) })
        .map(|()| Launched::Spawned)
        .map_err(|e| Refused::Failed(e.to_string()))
}

/// How to start the app from a process in a job with these limit flags (None: in no job): its
/// creation flags, or Err when it would be tied to the job and cannot break away.
fn spawn_flags(job: Option<u32>) -> Result<u32, ()> {
    match job {
        None => Ok(0),
        Some(f) if f & JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK != 0 => Ok(0),
        Some(f) if f & JOB_OBJECT_LIMIT_BREAKAWAY_OK != 0 => Ok(CREATE_BREAKAWAY_FROM_JOB),
        Some(f) if f & TYING != 0 => Err(()),
        Some(_) => Ok(0),
    }
}

/// The scheduled task's run: decides, records the decision (but not while SlopBar runs, so that
/// case ends at once) and starts SlopBar if it should. The exit code is the task's last result: 0
/// whatever it decided, 1 when a start failed, 2 when it would have been tied to the task's job.
pub fn check_with(machine: &dyn Machine, markers: Option<&Markers>, launcher: &dyn Launcher) -> i32 {
    if machine.running() {
        return 0;
    }
    // Without the markers, a person's Quit cannot be told: nothing is started.
    let Some(markers) = markers else { return 0 };
    let now = machine.now();
    let settings = machine.settings();
    let sign_in = machine.sign_in();
    let id = sign_in.as_ref().map_or("", |s| s.id.as_str());
    let app = machine.app_exe();
    let entry = app.as_ref().map_or(RunEntry::Elsewhere, |app| machine.run_entry(app));
    let facts = Facts {
        running: false,
        shutting_down: machine.shutting_down(),
        enabled: settings.keep_running,
        uninstalling: markers.age(UNINSTALLING, now),
        sign_in_age: sign_in.as_ref().map(|s| s.age),
        quit: quit_mark(markers.read(QUIT).as_deref(), id),
        started: started_mark(markers.read(STARTED).as_deref(), id),
        launches_at_login: launches_at_login(settings.launch_at_login, entry),
        installer: machine.installer_running(),
        installing: markers.age(INSTALLING, now),
        agent_quit: markers.age(AGENT_QUIT, now),
    };
    let verdict = decide(&facts);
    let exit = match verdict {
        Verdict::Start => match app {
            Ok(app) => start_with(markers, launcher, &app, &new_nonce()),
            Err(_) => 1,
        },
        _ => 0,
    };
    markers.record_check(verdict, exit);
    exit
}

/// Starts `app` with the handshake: the nonce is written first, so the start that presents it is
/// known as the watchdog's (a start without it is an ordinary one, which still runs SlopBar).
fn start_with(markers: &Markers, launcher: &dyn Launcher, app: &Path, nonce: &str) -> i32 {
    let _ = markers.write_handshake(nonce);
    let dir = app.parent().unwrap_or(Path::new("."));
    match launcher.launch(app, &[STARTED_FLAG.to_string(), nonce.to_string()], dir) {
        Ok(_) => 0,
        Err(Refused::Failed(_)) => 1,
        Err(Refused::Tied) => 2,
    }
}

fn new_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn check() -> i32 {
    check_with(&Windows, Markers::here().as_ref(), &ShellLauncher)
}

// ----------------------------------------------------------------------------------------------
// The start.

/// How this process was started, as far as the instance claim goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// By the watchdog (its handshake checked out).
    Watchdog,
    /// By the startup entry at sign-in (`sign_in_start`).
    SignIn,
    /// Any other way: the Start menu, a restart, an installer, a person, an agent.
    Other,
}

impl Start {
    /// Whether the start leaves quietly to an instance that already runs, as a restart does,
    /// instead of opening Settings there.
    pub fn is_quiet(self) -> bool {
        self != Start::Other
    }
}

/// The startup entry's start at sign-in: no arguments (the entry has none; the watchdog, a restart
/// and the installer's restart pass some), no shortcut (the Start menu, the taskbar and the desktop
/// start a shortcut, which Windows says in the start-up information), Explorer as the parent (it
/// runs the user's startup entries), and a sign-in that began less than SIGN_IN_START_WINDOW ago.
/// Only a person who starts slopbar.exe itself from a folder this soon after signing in looks the
/// same: if SlopBar already runs, that start ends quietly instead of opening Settings.
pub fn sign_in_start(no_args: bool, from_shortcut: bool, parent: Option<&str>, sign_in_age: Option<Duration>) -> bool {
    no_args && !from_shortcut && parent.is_some_and(|p| p.eq_ignore_ascii_case("explorer.exe")) && sign_in_age.is_some_and(|a| a < SIGN_IN_START_WINDOW)
}

/// The nonce a `--watchdog-start` start presents.
fn handshake_arg(args: &[OsString]) -> Option<String> {
    (args.get(1)? == STARTED_FLAG).then(|| args.get(2).and_then(|a| a.to_str()).unwrap_or_default().to_string())
}

/// How a start with `args` counts (`Start`), from the handshake or, for a start with no arguments,
/// from how Explorer started it.
pub fn start_kind_with(args: &[OsString], markers: Option<&Markers>, now: SystemTime, sign_in_start: impl FnOnce() -> bool) -> Start {
    match handshake_arg(args) {
        // The updater restarts an app with its old arguments: their nonce is spent by then.
        Some(nonce) => {
            if markers.is_some_and(|m| m.take_handshake(&nonce, now)) {
                Start::Watchdog
            } else {
                Start::Other
            }
        }
        None if args.len() == 1 && sign_in_start() => Start::SignIn,
        None => Start::Other,
    }
}

/// main.rs: how this start counts.
pub fn start_kind(args: &[OsString]) -> Start {
    start_kind_with(args, Markers::here().as_ref(), SystemTime::now(), || {
        sign_in_start(true, sys::started_from_shortcut(), crate::instance::parent_name().as_deref(), Windows.sign_in().map(|s| s.age))
    })
}

/// main.rs: runs the check, for the task, and returns its exit code; None for every other start.
/// Only the second argument counts, so a task given more arguments still never starts SlopBar as a
/// second launch, which would open Settings.
pub fn from_args(args: &[OsString]) -> Option<i32> {
    // Keeps the sentinel in every build that handles the flag (`handles_watchdog`).
    std::hint::black_box(SENTINEL);
    (args.get(1)? == FLAG).then(check)
}

static STARTED_BY_WATCHDOG: AtomicBool = AtomicBool::new(false);

/// main.rs, once this process holds the instance lock: records the start (`Markers::record_start`).
pub fn started(start: Start) {
    started_in(Markers::here().as_ref(), Windows.sign_in().map(|s| s.id).as_deref(), start);
}

/// Records a start in `markers`, and says (and keeps, for get_app_info) whether it was the
/// watchdog's.
fn started_in(markers: Option<&Markers>, sign_in: Option<&str>, start: Start) -> bool {
    STARTED_BY_WATCHDOG.store(start == Start::Watchdog, Ordering::Relaxed);
    if let Some(markers) = markers {
        markers.record_start(sign_in, start == Start::Watchdog);
    }
    start == Start::Watchdog
}

/// A person's Quit from SlopBar's own menu (the tray's or the widget's), before it exits.
pub fn quit_by_person() {
    let _ = quit_by_person_in(Markers::here().as_ref(), Windows.sign_in().map(|s| s.id).as_deref(), &|| remove_task(tasks(), task_name()));
}

/// How a person's Quit is held: by its marker, written (tried twice); else by removing the task, so
/// no check runs at all until a start that is not the watchdog's registers it again. Err when
/// neither worked.
fn quit_by_person_in(markers: Option<&Markers>, sign_in: Option<&str>, remove: &dyn Fn() -> Result<bool, String>) -> Result<QuitHeld, String> {
    if let Some(m) = markers {
        if m.record_quit(sign_in).or_else(|_| m.record_quit(sign_in)).is_ok() {
            return Ok(QuitHeld::Marker);
        }
    }
    remove().map(|_| QuitHeld::NoTask)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitHeld {
    Marker,
    NoTask,
}

/// The door's quit_app, which is no person's Quit: the check starts SlopBar again after its grace.
pub fn quit_by_agent() {
    quit_by_agent_in(Markers::here().as_ref());
}

fn quit_by_agent_in(markers: Option<&Markers>) {
    if let Some(m) = markers {
        m.record_agent_quit();
    }
}

/// Right before an update hands over to its installer: the check leaves the installer alone.
pub fn installing(version: &str) {
    installing_in(Markers::here().as_ref(), version);
}

fn installing_in(markers: Option<&Markers>, version: &str) {
    if let Some(m) = markers {
        m.record_install(version);
    }
}

// ----------------------------------------------------------------------------------------------
// The task's definition.

/// Whether this build keeps the task.
fn manages() -> bool {
    builds_task(cfg!(feature = "ultra-updates"), cfg!(debug_assertions), crate::settings::isolated())
}

/// Only Ultra release builds keep the task, and not on `SLOPBAR_DATA_DIR`: a debug build is a
/// console app, which would show a console window at every run, other builds take no Ultra
/// releases, and an isolated run leaves the installed app's task alone.
const fn builds_task(ultra_updates: bool, debug: bool, isolated: bool) -> bool {
    ultra_updates && !debug && !isolated
}

/// This user's task.
fn task_name() -> Option<String> {
    sys::user_sid().map(|sid| task_name_for(&sid))
}

fn task_name_for(sid: &str) -> String {
    format!("{TASK_NAME} {sid}")
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
    <Priority>{PRIORITY}</Priority>
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

/// An element of a task's definition: its name (without a prefix), its attributes, its text and
/// its elements.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Element {
    name: String,
    attributes: Vec<(String, String)>,
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

    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// The root element of `xml`, read as much of XML as a task's definition uses: a declaration,
/// comments, attributes, and the five entities and character references. None when it is not well
/// formed, or uses more (a DTD, CDATA).
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
            let tag = tag.trim_end_matches('/');
            let name = local(tag.split(char::is_whitespace).next()?);
            if name.is_empty() {
                return None;
            }
            let element = Element { name: name.into(), attributes: attributes(&tag[tag.find(char::is_whitespace).unwrap_or(tag.len())..])?, ..Element::default() };
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

/// A start tag's attributes (`name="value"` or `name='value'`), names without a prefix.
fn attributes(mut text: &str) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    loop {
        text = text.trim_start();
        if text.is_empty() {
            return Some(out);
        }
        let (name, rest) = text.split_once('=')?;
        let rest = rest.trim_start();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let end = rest[1..].find(quote)? + 1;
        out.push((local(name.trim()).to_string(), unescape(&rest[1..end])?));
        text = &rest[end + 1..];
    }
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
    pub start: Option<String>,
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
    /// The principal the actions run as names the task's only principal.
    pub context_ok: bool,
    pub principals: usize,
    pub user: Option<String>,
    pub logon: String,
    pub run_level: String,
    pub triggers: Vec<Trigger>,
    pub instances: String,
    pub time_limit: String,
    pub priority: u8,
    pub starts_on_batteries: bool,
    pub stops_on_batteries: bool,
    pub enabled: bool,
    pub only_if_idle: bool,
    pub only_if_network: bool,
    pub wakes: bool,
    pub on_demand: bool,
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
    let actions_element = task.child("Actions");
    let actions = actions_element.map_or_else(Vec::new, |a| {
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
    let context = actions_element.and_then(|a| a.attribute("Context"));
    let triggers = task.child("Triggers").map_or_else(Vec::new, |t| {
        t.children
            .iter()
            .map(|trigger| {
                let repetition = trigger.child("Repetition");
                Trigger {
                    kind: trigger.name.clone(),
                    enabled: trigger.text_of("Enabled").is_none_or(|t| t == "true" || t == "1"),
                    start: trigger.text_of("StartBoundary").map(str::to_string),
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
        context_ok: context.is_none_or(|c| principal.and_then(|p| p.attribute("id")) == Some(c)),
        principals: principals.len(),
        user: principal.and_then(|p| p.text_of("UserId")).map(str::to_string),
        logon: principal.and_then(|p| p.text_of("LogonType")).unwrap_or_default().to_string(),
        run_level: principal.and_then(|p| p.text_of("RunLevel")).unwrap_or("LeastPrivilege").to_string(),
        triggers,
        instances: setting("MultipleInstancesPolicy").unwrap_or("IgnoreNew").to_string(),
        time_limit: setting("ExecutionTimeLimit").unwrap_or("PT72H").to_string(),
        priority: setting("Priority").map_or(Some(7), |p| p.parse().ok()).unwrap_or(u8::MAX),
        starts_on_batteries: !flag("DisallowStartIfOnBatteries", true),
        stops_on_batteries: flag("StopIfGoingOnBatteries", true),
        enabled: flag("Enabled", true),
        only_if_idle: flag("RunOnlyIfIdle", false),
        only_if_network: flag("RunOnlyIfNetworkAvailable", false),
        wakes: flag("WakeToRun", false),
        on_demand: flag("AllowStartOnDemand", true),
    })
}

/// Whether a person turned the task, or its trigger, off in Task Scheduler: that is left as it is.
pub fn disabled_by_hand(found: &Shape) -> bool {
    !found.enabled || found.triggers.iter().any(|t| !t.enabled)
}

/// How `found` differs from `expected`, in words; empty when the task is as it should be (whether
/// it is turned off is `disabled_by_hand`'s, not this). `user` is this user's SID: a task that runs
/// as anyone else is not this user's.
pub fn problems(found: &Shape, expected: &Shape, user: Option<&str>) -> Vec<String> {
    let mut out = Vec::new();
    let same_action = |a: &(String, String), b: &(String, String)| a.0.to_lowercase() == b.0.to_lowercase() && a.1 == b.1;
    let actions_match = found.actions.len() == expected.actions.len() && found.actions.iter().zip(&expected.actions).all(|(a, b)| same_action(a, b));
    if !actions_match {
        let said: Vec<String> = found.actions.iter().map(|(c, a)| format!("{} {a}", file_name(c)).trim().to_string()).collect();
        out.push(format!("runs {}, not this app's {FLAG}", if said.is_empty() { "nothing".to_string() } else { said.join(" and ") }));
    }
    if !found.context_ok {
        out.push("its actions name no principal of its own".into());
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
    let enabled = |t: &Trigger| Trigger { enabled: true, ..t.clone() };
    if found.triggers.iter().map(enabled).collect::<Vec<_>>() != expected.triggers {
        let said: Vec<String> = found
            .triggers
            .iter()
            .map(|t| {
                let mut words = vec![t.kind.clone()];
                words.push(t.interval.as_ref().map_or("not repeating".into(), |i| format!("every {i}")));
                if let Some(d) = &t.duration {
                    words.push(format!("for {d}"));
                }
                if t.start != expected.triggers.first().and_then(|e| e.start.clone()) {
                    words.push(format!("from {}", t.start.as_deref().unwrap_or("no start")));
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
    if found.priority != expected.priority {
        out.push(format!("priority {}, not {}", found.priority, expected.priority));
    }
    if found.starts_on_batteries != expected.starts_on_batteries || found.stops_on_batteries != expected.stops_on_batteries {
        out.push("held back or stopped on batteries".into());
    }
    if found.only_if_idle != expected.only_if_idle || found.only_if_network != expected.only_if_network {
        out.push("runs only when idle or on a network".into());
    }
    if found.wakes != expected.wakes {
        out.push("wakes the computer".into());
    }
    if found.on_demand != expected.on_demand {
        out.push("cannot be run on demand".into());
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
pub enum Judged {
    Ok,
    Missing,
    /// A person turned it off in Task Scheduler.
    Disabled,
    Wrong(Vec<String>),
}

pub fn judge(found: Option<&str>, expected: &Shape, user: Option<&str>) -> Judged {
    let Some(xml) = found else { return Judged::Missing };
    match shape(xml) {
        None => Judged::Wrong(vec!["its definition cannot be read".into()]),
        Some(found) if disabled_by_hand(&found) => Judged::Disabled,
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

/// An OLE date (days since 1899-12-30, in the time zone `zone`) as UTC; None before 1900, which
/// Task Scheduler gives for a task that has not run.
fn ole_date_in<Z: chrono::TimeZone>(days: f64, zone: &Z) -> Option<chrono::DateTime<chrono::Utc>> {
    if !(days >= 1.0 && days < 2_958_466.0) {
        return None;
    }
    let base = chrono::NaiveDate::from_ymd_opt(1899, 12, 30)?.and_hms_opt(0, 0, 0)?;
    let local = base + chrono::Duration::milliseconds((days * 86_400_000.0).round() as i64);
    zone.from_local_datetime(&local).earliest().map(|t| t.with_timezone(&chrono::Utc))
}

// ----------------------------------------------------------------------------------------------
// Task Scheduler.

/// A registered task: its definition and its last run (an OLE date, and its result).
#[derive(Debug, Clone, PartialEq)]
pub struct Registered {
    pub xml: String,
    pub last_run: f64,
    pub last_result: i32,
}

/// Task Scheduler's tasks, by name.
pub trait Scheduler: Send + Sync {
    /// The task named `name`; None when there is none.
    fn read(&self, name: &str) -> Result<Option<Registered>, String>;
    /// Creates the task named `name`, or replaces it, as `xml` defines it, for this user while
    /// signed in (interactive token: no password is stored).
    fn register(&self, name: &str, xml: &str) -> Result<(), String>;
    /// Removes the task named `name`: whether there was one.
    fn delete(&self, name: &str) -> Result<bool, String>;
}

/// Calls run one at a time, in the order they came, on a thread of their own, each waited for at
/// most `limit`. A call whose wait ended still runs in its turn, before any that came after it, so
/// a slow registration can never re-create a task after a later removal.
struct Queue<B: 'static> {
    jobs: Mutex<Option<mpsc::Sender<Box<dyn FnOnce(&mut B) + Send>>>>,
    make: fn() -> B,
    limit: Duration,
}

impl<B: 'static> Queue<B> {
    const fn new(make: fn() -> B, limit: Duration) -> Self {
        Queue { jobs: Mutex::new(None), make, limit }
    }

    fn call<T: Send + 'static>(&self, job: impl FnOnce(&mut B) -> T + Send + 'static) -> Result<T, String> {
        let (done, answer) = mpsc::channel();
        {
            let mut jobs = lock(&self.jobs);
            if jobs.is_none() {
                let (send, receive) = mpsc::channel::<Box<dyn FnOnce(&mut B) + Send>>();
                let make = self.make;
                std::thread::Builder::new()
                    .name("keep-running-scheduler".into())
                    .spawn(move || {
                        let mut backend = make();
                        while let Ok(job) = receive.recv() {
                            job(&mut backend);
                        }
                    })
                    .map_err(|e| e.to_string())?;
                *jobs = Some(send);
            }
            let job: Box<dyn FnOnce(&mut B) + Send> = Box::new(move |b: &mut B| {
                let _ = done.send(job(b));
            });
            jobs.as_ref().map(|s| s.send(job)).transpose().map_err(|_| "Task Scheduler's queue stopped".to_string())?;
        }
        answer.recv_timeout(self.limit).map_err(|_| format!("Task Scheduler did not answer within {} s", self.limit.as_secs()))
    }
}

/// What a queue's thread holds while it talks to Task Scheduler.
pub trait Backend {
    fn read(&mut self, name: &str) -> Result<Option<Registered>, String>;
    fn register(&mut self, name: &str, xml: &str) -> Result<(), String>;
    fn delete(&mut self, name: &str) -> Result<bool, String>;
}

impl<B: Backend + 'static> Scheduler for Queue<B> {
    fn read(&self, name: &str) -> Result<Option<Registered>, String> {
        let name = name.to_string();
        self.call(move |b| b.read(&name))?
    }
    fn register(&self, name: &str, xml: &str) -> Result<(), String> {
        let (name, xml) = (name.to_string(), xml.to_string());
        self.call(move |b| b.register(&name, &xml))?
    }
    fn delete(&self, name: &str) -> Result<bool, String> {
        let name = name.to_string();
        self.call(move |b| b.delete(&name))?
    }
}

/// This user's Task Scheduler.
fn tasks() -> &'static dyn Scheduler {
    static TASKS: Queue<sys::TaskService> = Queue::new(sys::TaskService::start, SCHEDULER_LIMIT);
    &TASKS
}

/// Removes this user's task (`name`): whether there was one.
fn remove_task(scheduler: &dyn Scheduler, name: Option<String>) -> Result<bool, String> {
    scheduler.delete(&name.ok_or("this user's SID could not be read")?)
}

// ----------------------------------------------------------------------------------------------
// Keeping the task.

/// What a change of the task came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Synced {
    Registered,
    /// It was there but differed, so it was removed and registered anew.
    Repaired,
    AlreadyOk,
    /// A person turned it off in Task Scheduler: left as it is.
    LeftDisabled,
    Removed,
    /// An uninstall is under way: nothing registered.
    HeldByUninstall,
}

/// What the task is kept against.
pub struct TaskEnv {
    pub name: Option<String>,
    pub sid: Option<String>,
    pub exe: Result<PathBuf, String>,
    pub uninstalling: bool,
}

impl TaskEnv {
    fn here() -> Self {
        let sid = sys::user_sid();
        TaskEnv {
            name: sid.as_deref().map(task_name_for),
            sid,
            exe: Windows.app_exe(),
            uninstalling: Markers::here().is_some_and(|m| m.uninstalling(SystemTime::now())),
        }
    }
}

/// Makes the task match `enabled`: removes it while off; while on, registers it (never for a
/// console app, never during an uninstall), repairs one that differs by removing it and registering
/// it anew (a corrupt or foreign task too), and leaves one a person turned off as it is.
pub fn sync(scheduler: &dyn Scheduler, env: &TaskEnv, enabled: bool) -> Result<Synced, String> {
    let name = env.name.as_deref().ok_or("this user's SID could not be read")?;
    if !enabled {
        return scheduler.delete(name).map(|_| Synced::Removed);
    }
    if env.uninstalling {
        return Ok(Synced::HeldByUninstall);
    }
    let exe = env.exe.as_ref().map_err(String::clone)?;
    // Never a console app: it would show a console window at every run.
    if !is_window_app(exe) {
        return Err(format!("{} is not a window app", file_name(&exe.to_string_lossy())));
    }
    let xml = task_xml(exe);
    let expected = shape(&xml).ok_or("this app's own task definition cannot be read")?;
    let found = scheduler.read(name)?;
    match judge(found.as_ref().map(|t| t.xml.as_str()), &expected, env.sid.as_deref()) {
        Judged::Ok => Ok(Synced::AlreadyOk),
        Judged::Disabled => Ok(Synced::LeftDisabled),
        Judged::Missing => scheduler.register(name, &xml).map(|()| Synced::Registered),
        Judged::Wrong(_) => {
            scheduler.delete(name)?;
            scheduler.register(name, &xml).map(|()| Synced::Repaired)
        }
    }
}

/// One change of the task at a time, retried after a failure (`RETRIES`), and the last outcome.
pub struct Syncer {
    state: Mutex<SyncState>,
    wake: Condvar,
}

#[derive(Default)]
struct SyncState {
    wanted: Option<bool>,
    generation: u64,
    running: bool,
    outcome: Option<Result<Synced, String>>,
}

type SyncWork = Arc<dyn Fn(bool) -> Result<Synced, String> + Send + Sync>;

impl Syncer {
    pub fn new() -> Arc<Self> {
        Arc::new(Syncer { state: Mutex::new(SyncState::default()), wake: Condvar::new() })
    }

    /// Asks for the task to match `enabled`, by `work` on a thread of its own: at once, then after
    /// each of `delays` while it fails, and never again for the same value once it has succeeded.
    pub fn apply(self: &Arc<Self>, enabled: bool, work: SyncWork, delays: &'static [Duration]) {
        let mut state = lock(&self.state);
        if state.wanted == Some(enabled) && (state.running || matches!(state.outcome, Some(Ok(_)))) {
            return;
        }
        state.wanted = Some(enabled);
        state.generation += 1;
        if state.running {
            self.wake.notify_all();
            return;
        }
        state.running = true;
        let me = self.clone();
        drop(state);
        let spawned = std::thread::Builder::new().name("keep-running".into()).spawn(move || me.run(work, delays));
        if spawned.is_err() {
            lock(&self.state).running = false;
        }
    }

    fn run(&self, work: SyncWork, delays: &'static [Duration]) {
        let mut tries = 0usize;
        loop {
            let (generation, wanted) = {
                let state = lock(&self.state);
                (state.generation, state.wanted.unwrap_or(true))
            };
            let outcome = work(wanted);
            let mut state = lock(&self.state);
            let failed = outcome.is_err();
            state.outcome = Some(outcome);
            if state.generation != generation {
                tries = 0;
                continue;
            }
            if !failed || tries >= delays.len() {
                state.running = false;
                self.wake.notify_all();
                return;
            }
            let delay = delays[tries];
            tries += 1;
            let (state, _) = self.wake.wait_timeout_while(state, delay, |s| s.generation == generation).unwrap_or_else(|e| e.into_inner());
            if state.generation != generation {
                tries = 0;
            }
        }
    }

    /// Whether a change is under way.
    pub fn syncing(&self) -> bool {
        lock(&self.state).running
    }

    pub fn outcome(&self) -> Option<Result<Synced, String>> {
        lock(&self.state).outcome.clone()
    }

    /// Waits for the change under way to end, at most `limit`: whether it did.
    #[cfg(test)]
    pub fn wait_idle(&self, limit: Duration) -> bool {
        let state = lock(&self.state);
        let (state, _) = self.wake.wait_timeout_while(state, limit, |s| s.running).unwrap_or_else(|e| e.into_inner());
        !state.running
    }
}

fn syncer() -> &'static Arc<Syncer> {
    static SYNCER: OnceLock<Arc<Syncer>> = OnceLock::new();
    SYNCER.get_or_init(Syncer::new)
}

/// app.rs (`apply_settings`, at startup and after every settings change): makes the task match
/// `keep_running`, when this build keeps it.
pub fn apply(enabled: bool) {
    apply_with(manages(), syncer(), enabled, Arc::new(|enabled| sync(tasks(), &TaskEnv::here(), enabled)), &RETRIES);
}

fn apply_with(manages: bool, syncer: &Arc<Syncer>, enabled: bool, work: SyncWork, delays: &'static [Duration]) {
    if manages {
        syncer.apply(enabled, work, delays);
    }
}

// ----------------------------------------------------------------------------------------------
// get_app_info.

/// What the report says besides what Task Scheduler answers.
pub struct ReportInputs {
    pub enabled: bool,
    pub managed: bool,
    pub syncing: bool,
    pub outcome: Option<Result<Synced, String>>,
    pub env: TaskEnv,
    pub started_by_watchdog: bool,
    pub last_check: Option<LastCheck>,
    pub launch_at_login: bool,
    pub run_entry: RunEntry,
}

/// get_app_info's `keep_running`: the setting, whether this build keeps the task, the task as it is
/// (ok; pending while it is being made; missing; disabled by hand; wrong with its problems; or
/// unknown with the error), its last run, the check's own last decision and exit code, and how
/// Windows starts SlopBar at sign-in.
pub fn report_with<Z: chrono::TimeZone>(scheduler: &dyn Scheduler, inputs: &ReportInputs, zone: &Z) -> serde_json::Value {
    let read = inputs.env.name.as_deref().ok_or_else(|| "this user's SID could not be read".to_string()).and_then(|name| scheduler.read(name));
    let expected = inputs.env.exe.as_ref().ok().and_then(|exe| shape(&task_xml(exe)));
    let failed = inputs.outcome.as_ref().and_then(|o| o.as_ref().err().cloned());
    let (task, problems, error, last_run, last_result) = match (read, expected) {
        (Err(e), _) => ("unknown", vec![], Some(e), None, None),
        (Ok(_), None) => ("unknown", vec![], Some("this app's own task definition cannot be read".to_string()), None, None),
        (Ok(found), Some(expected)) => {
            let (run, result) = found.as_ref().map_or((None, None), |t| (ole_date_in(t.last_run, zone), Some(t.last_result)));
            match judge(found.as_ref().map(|t| t.xml.as_str()), &expected, inputs.env.sid.as_deref()) {
                Judged::Ok => ("ok", vec![], failed, run, result),
                Judged::Missing if inputs.syncing => ("pending", vec![], failed, None, None),
                Judged::Missing => ("missing", vec![], failed, None, None),
                Judged::Disabled => ("disabled", vec![], failed, run, result),
                Judged::Wrong(p) if inputs.syncing => ("pending", p, failed, run, result),
                Judged::Wrong(p) => ("wrong", p, failed, run, result),
            }
        }
    };
    let stamp = |t: SystemTime| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    serde_json::json!({
        "enabled": inputs.enabled,
        "managed": inputs.managed,
        "task": task,
        "problems": problems,
        "error": error,
        "last_run": last_run.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "last_result": last_result,
        "last_check": inputs.last_check.as_ref().map(|c| serde_json::json!({ "at": stamp(c.at), "decision": c.decision, "exit": c.exit })),
        "launch_at_login": { "setting": inputs.launch_at_login, "entry": inputs.run_entry.name() },
        "started_by_watchdog": inputs.started_by_watchdog,
    })
}

/// get_app_info's `keep_running`, for the settings `keep_running` and `launch_at_login`.
pub fn report(enabled: bool, launch_at_login: bool) -> serde_json::Value {
    let env = TaskEnv::here();
    let run_entry = env.exe.as_ref().map_or(RunEntry::Elsewhere, |exe| Windows.run_entry(exe));
    let inputs = ReportInputs {
        enabled,
        managed: manages(),
        syncing: syncer().syncing(),
        outcome: syncer().outcome(),
        env,
        started_by_watchdog: STARTED_BY_WATCHDOG.load(Ordering::Relaxed),
        last_check: Markers::here().and_then(|m| m.last_check(SystemTime::now())),
        launch_at_login,
        run_entry,
    };
    report_with(tasks(), &inputs, &chrono::Local)
}

// ----------------------------------------------------------------------------------------------
// Restarting into another build.

/// Whether the executable at `exe` handles `--keep-running` (it holds the sentinel). One that
/// cannot be read is taken to: it cannot be started either.
pub fn handles_watchdog(exe: &Path) -> bool {
    contains(exe, SENTINEL.as_bytes()).unwrap_or(true)
}

/// Whether the file at `path` holds `needle`, read a chunk at a time.
fn contains(path: &Path, needle: &[u8]) -> std::io::Result<bool> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut window = Vec::with_capacity(1 << 20);
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            return Ok(false);
        }
        window.extend_from_slice(&chunk[..n]);
        if window.windows(needle.len()).any(|w| w == needle) {
            return Ok(true);
        }
        // Keep the tail, which may hold the start of the needle.
        let keep = window.len().min(needle.len().saturating_sub(1));
        window.drain(..window.len() - keep);
    }
}

/// app.rs (`restart`), before it restarts into `exe`: an executable without the watchdog would take
/// the task's runs for plain second launches (Settings opening every 5 minutes), so the task goes
/// first. The next build with the watchdog registers it again.
pub fn before_restart(exe: &Path) {
    before_restart_with(exe, &|| remove_task(tasks(), task_name()));
}

fn before_restart_with(exe: &Path, remove: &dyn Fn() -> Result<bool, String>) -> Option<Result<bool, String>> {
    (!handles_watchdog(exe)).then(remove)
}

// ----------------------------------------------------------------------------------------------
// Windows.

#[cfg(windows)]
mod sys {
    use super::RunEntry;
    use std::path::Path;
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
            Some(Session { id: info.SessionId, logon: info.LogonTime, now: info.CurrentTime })
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

    /// Whether Windows is signing out or shutting down.
    pub fn shutting_down() -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SHUTTINGDOWN};
        // SAFETY: no arguments to get wrong.
        unsafe { GetSystemMetrics(SM_SHUTTINGDOWN) != 0 }
    }

    /// Whether this process was started from a shortcut (the Start menu, the taskbar, the desktop).
    pub fn started_from_shortcut() -> bool {
        use windows_sys::Win32::System::Threading::{GetStartupInfoW, STARTF_TITLEISLINKNAME, STARTUPINFOW};
        // SAFETY: an out-parameter sized for itself.
        unsafe {
            let mut info: STARTUPINFOW = std::mem::zeroed();
            info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            GetStartupInfoW(&mut info);
            info.dwFlags & STARTF_TITLEISLINKNAME != 0
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// A registry value's bytes, with its type (`flags` picks the types RegGetValueW accepts).
    fn value(root: windows_sys::Win32::System::Registry::HKEY, key: &str, name: &str, flags: u32) -> Option<Vec<u8>> {
        use windows_sys::Win32::System::Registry::RegGetValueW;
        let (key, name) = (wide(key), wide(name));
        let mut size = 0u32;
        // SAFETY: a size query, then a read into a buffer of that size.
        unsafe {
            if RegGetValueW(root, key.as_ptr(), name.as_ptr(), flags, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) != 0 {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            if RegGetValueW(root, key.as_ptr(), name.as_ptr(), flags, std::ptr::null_mut(), data.as_mut_ptr().cast(), &mut size) != 0 {
                return None;
            }
            data.truncate(size as usize);
            Some(data)
        }
    }

    fn string(root: windows_sys::Win32::System::Registry::HKEY, key: &str, name: &str) -> Option<String> {
        use windows_sys::Win32::System::Registry::{RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ};
        let data = value(root, key, name, RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND)?;
        let wide: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|c| *c != 0).collect();
        Some(String::from_utf16_lossy(&wide))
    }

    /// The startup entry, as Windows will treat it at sign-in (`super::run_entry_from`).
    pub fn run_entry(app: &Path) -> RunEntry {
        use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RRF_RT_REG_DWORD};
        const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
        const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
        const POLICY: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
        let blocked = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().any(|root| {
            value(root, POLICY, "DisableCurrentUserRun", RRF_RT_REG_DWORD).is_some_and(|d| d.iter().any(|b| *b != 0))
        });
        let command = string(HKEY_CURRENT_USER, RUN, super::RUN_NAME);
        let approved = value(HKEY_CURRENT_USER, APPROVED, super::RUN_NAME, RRF_RT_REG_BINARY);
        super::run_entry_from(command.as_deref(), approved.as_deref(), blocked, |p| super::same_file(p, app))
    }

    /// A VARIANT holding a 32-bit integer.
    unsafe fn variant_i32(v: i32) -> windows::Win32::System::Variant::VARIANT {
        use windows::Win32::System::Variant::{VARIANT, VT_I4};
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VT_I4;
        (*var.Anonymous.Anonymous).Anonymous.lVal = v;
        var
    }

    /// A VARIANT holding a BSTR, which VariantClear frees.
    unsafe fn variant_text(s: &str) -> windows::Win32::System::Variant::VARIANT {
        use windows::Win32::System::Variant::{VARIANT, VT_BSTR};
        let mut var = VARIANT::default();
        (*var.Anonymous.Anonymous).vt = VT_BSTR;
        (*var.Anonymous.Anonymous).Anonymous.bstrVal = std::mem::ManuallyDrop::new(windows::core::BSTR::from(s));
        var
    }

    /// Starts `exe` with `args` in `dir` the way the shell starts programs: through the desktop's
    /// Explorer (its ShellWindows, the desktop's view, and that view's Shell.Application), so the
    /// program is Explorer's child, at Explorer's priority, and in none of the caller's jobs.
    pub fn shell_execute(exe: &Path, args: &str, dir: &Path) -> Result<(), String> {
        use windows::core::{Interface, BSTR};
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch, IServiceProvider, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
        use windows::Win32::System::Variant::{VariantClear, VARIANT};
        use windows::Win32::UI::Shell::{
            IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellView, IShellWindows, ShellWindows, CSIDL_DESKTOP, SID_STopLevelBrowser, SVGIO_BACKGROUND,
            SWC_DESKTOP, SWFO_NEEDDISPATCH,
        };
        /// SW_SHOWNORMAL.
        const SHOW_NORMAL: i32 = 1;
        // SAFETY: COM is started on this thread for the call and ended once every object made here
        // has been released (they are all locals of `run`); each VARIANT is cleared once.
        unsafe {
            let started = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
            let run = || -> windows::core::Result<()> {
                let windows: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)?;
                let mut desktop = variant_i32(CSIDL_DESKTOP as i32);
                let empty = VARIANT::default();
                let mut hwnd = 0i32;
                let found = windows.FindWindowSW(&desktop, &empty, SWC_DESKTOP, &mut hwnd, SWFO_NEEDDISPATCH);
                let _ = VariantClear(&mut desktop);
                let browser: IShellBrowser = found?.cast::<IServiceProvider>()?.QueryService(&SID_STopLevelBrowser)?;
                let view: IShellView = browser.QueryActiveShellView()?;
                let background: IDispatch = view.GetItemObject(SVGIO_BACKGROUND)?;
                let shell: IShellDispatch2 = background.cast::<IShellFolderViewDual>()?.Application()?.cast()?;
                let mut values = [variant_text(args), variant_text(&dir.to_string_lossy()), variant_text("open"), variant_i32(SHOW_NORMAL)];
                let done = shell.ShellExecute(&BSTR::from(exe.to_string_lossy().as_ref()), &values[0], &values[1], &values[2], &values[3]);
                for v in values.iter_mut() {
                    let _ = VariantClear(v);
                }
                done
            };
            let result = run();
            if started {
                CoUninitialize();
            }
            result.map_err(|e| format!("the shell could not start it: {:#010x} {}", e.code().0, e.message().trim()))
        }
    }

    /// Task Scheduler through its COM interface: no console program to start (schtasks would print
    /// the definition in the console's code page), and the definition read back exactly. COM is
    /// started on the queue's thread, which makes every call (`super::Queue`).
    pub struct TaskService;

    impl TaskService {
        pub fn start() -> Self {
            // SAFETY: once, on the queue's own thread, which lives as long as the process.
            let _ = unsafe { windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_MULTITHREADED) };
            TaskService
        }

        fn folder() -> windows::core::Result<windows::Win32::System::TaskScheduler::ITaskFolder> {
            use windows::core::BSTR;
            use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
            use windows::Win32::System::TaskScheduler::{ITaskService, TaskScheduler};
            use windows::Win32::System::Variant::VARIANT;
            // SAFETY: COM calls on interfaces this thread made.
            unsafe {
                let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
                let none = VARIANT::default();
                service.Connect(&none, &none, &none, &none)?;
                service.GetFolder(&BSTR::from("\\"))
            }
        }
    }

    /// HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND): no such task.
    const NOT_FOUND: i32 = 0x8007_0002_u32 as i32;

    fn describe(e: &windows::core::Error) -> String {
        format!("Task Scheduler: {:#010x} {}", e.code().0, e.message().trim())
    }

    impl super::Backend for TaskService {
        fn read(&mut self, name: &str) -> Result<Option<super::Registered>, String> {
            use windows::core::BSTR;
            // SAFETY: COM calls on interfaces this thread made.
            unsafe {
                let folder = Self::folder().map_err(|e| describe(&e))?;
                match folder.GetTask(&BSTR::from(name)) {
                    Err(e) if e.code().0 == NOT_FOUND => Ok(None),
                    Err(e) => Err(describe(&e)),
                    Ok(task) => (|| -> windows::core::Result<Option<super::Registered>> {
                        Ok(Some(super::Registered { xml: task.Xml()?.to_string(), last_run: task.LastRunTime()?, last_result: task.LastTaskResult()? }))
                    })()
                    .map_err(|e| describe(&e)),
                }
            }
        }

        fn register(&mut self, name: &str, xml: &str) -> Result<(), String> {
            use windows::core::BSTR;
            use windows::Win32::System::TaskScheduler::{TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN};
            use windows::Win32::System::Variant::VARIANT;
            // SAFETY: COM calls on interfaces this thread made.
            unsafe {
                let folder = Self::folder().map_err(|e| describe(&e))?;
                let none = VARIANT::default();
                folder
                    .RegisterTask(&BSTR::from(name), &BSTR::from(xml), TASK_CREATE_OR_UPDATE.0, &none, &none, TASK_LOGON_INTERACTIVE_TOKEN, &none)
                    .map(drop)
                    .map_err(|e| describe(&e))
            }
        }

        fn delete(&mut self, name: &str) -> Result<bool, String> {
            use windows::core::BSTR;
            // SAFETY: COM calls on interfaces this thread made.
            unsafe {
                let folder = Self::folder().map_err(|e| describe(&e))?;
                match folder.DeleteTask(&BSTR::from(name), 0) {
                    Ok(()) => Ok(true),
                    Err(e) if e.code().0 == NOT_FOUND => Ok(false),
                    Err(e) => Err(describe(&e)),
                }
            }
        }
    }
}

#[cfg(not(windows))]
mod sys {
    use super::RunEntry;
    use std::path::Path;
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
    pub fn job_limits() -> Option<u32> {
        None
    }
    pub fn shutting_down() -> bool {
        false
    }
    pub fn started_from_shortcut() -> bool {
        false
    }
    pub fn run_entry(_app: &Path) -> RunEntry {
        RunEntry::Missing
    }
    pub fn shell_execute(_exe: &Path, _args: &str, _dir: &Path) -> Result<(), String> {
        Err("no shell".into())
    }
    pub struct TaskService;
    impl TaskService {
        pub fn start() -> Self {
            TaskService
        }
    }
    const UNSUPPORTED: &str = "the watchdog's task is Windows only";
    impl super::Backend for TaskService {
        fn read(&mut self, _name: &str) -> Result<Option<super::Registered>, String> {
            Err(UNSUPPORTED.into())
        }
        fn register(&mut self, _name: &str, _xml: &str) -> Result<(), String> {
            Err(UNSUPPORTED.into())
        }
        fn delete(&mut self, _name: &str) -> Result<bool, String> {
            Err(UNSUPPORTED.into())
        }
    }
}

#[cfg(test)]
mod tests;
