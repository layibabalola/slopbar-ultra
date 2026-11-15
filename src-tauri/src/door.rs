//! A local door into the running app, so agents can do what the Settings window does without
//! driving the UI (see `mcp`, served by `slopbar-mcp.exe`).
//!
//! The running app listens on a per-user named pipe and answers one JSON request per connection
//! with the same operations the Settings window uses. Remote clients are rejected and the pipe's
//! ACL admits only the signed-in user and SYSTEM. The client connects for identification only (the
//! server can never act as the client), checks that the process serving the pipe runs as the same
//! user, bounds the reply in size and time, and only accepts an export path inside the export
//! folder. Sign-ins started here open the CLI's own login for the user; no credential ever crosses
//! the pipe.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::accounts::Provider;

/// Bumped when the request or reply shape changes.
pub const VERSION: u32 = 4;
/// Requests are one JSON line (a settings patch with aliases is the largest).
const MAX_REQUEST: u64 = 16 * 1024;
/// Replies are one JSON line (a usage snapshot is the largest).
const MAX_REPLY: u64 = 4 * 1024 * 1024;
/// A repeated export request within this window gets the export just written.
pub const REUSE_WINDOW: Duration = Duration::from_secs(30);
/// How long the client waits for a reply: under MCP clients' usual 60-second tool timeout, so the
/// agent hears "still working" rather than nothing. The app finishes the operation regardless.
pub(crate) const CLIENT_TIMEOUT: Duration = Duration::from_secs(55);

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    ExportDiagnostics,
    GetUsage,
    Refresh,
    GetSettings,
    /// Each account's plan, weekly usage history, verdict and next renewal (see `subscriptions`).
    GetSubscriptions,
    /// Plan changes per provider: cancels and downgrades with savings (see `advisor`).
    GetPlanAdvice,
    /// Accounts seen burning through their week abnormally fast in the past day (see `burn`).
    GetBurnAlerts,
    /// Tokens by account and repository, model or day from the CLIs' files here (see `usage_index`).
    GetUsageBreakdown {
        #[serde(default)]
        provider: Option<Provider>,
        #[serde(default)]
        account: Option<String>,
        #[serde(default)]
        days: Option<u32>,
        #[serde(default)]
        group_by: Option<crate::usage_index::GroupBy>,
    },
    /// Weekly history, the usage breakdown and the subscription report as CSV files.
    ExportCsv {
        #[serde(default)]
        folder: Option<String>,
    },
    /// Settings, usage history and the spend ledger in one file encrypted with the user's
    /// passphrase (see `backup`). No sign-ins.
    ExportBackup {
        #[serde(default)]
        path: Option<String>,
        passphrase: crate::backup::Passphrase,
    },
    /// Merge a backup file in; with `dry_run` (the default) only says what would change.
    RestoreBackup {
        path: String,
        passphrase: crate::backup::Passphrase,
        #[serde(default = "yes")]
        dry_run: bool,
    },
    /// An RFC 7386 merge patch over the current settings (see `settings::patched`).
    UpdateSettings { patch: Value },
    /// Used share, reset, pace and forecast per window for one account (or each provider's account
    /// in use), from the last snapshot (see `headroom`).
    GetHeadroom {
        #[serde(default)]
        provider: Option<Provider>,
        #[serde(default)]
        id: Option<String>,
    },
    /// The provider's accounts ranked for a job; with `switch`, moves to the best one as
    /// `SwitchAccount` does.
    RecommendAccount {
        provider: Provider,
        #[serde(default)]
        expected_percent: Option<f64>,
        #[serde(default)]
        hours: Option<f64>,
        #[serde(default)]
        switch: bool,
        /// The caller's own lease: its account is not excluded as leased (see `leases`).
        #[serde(default)]
        lease_id: Option<String>,
    },
    /// Reserve an account of the provider for a job (see `leases`): `id`, else the account
    /// recommend_account names among those nobody else leased.
    LeaseAccount {
        provider: Provider,
        purpose: String,
        #[serde(default)]
        minutes: Option<u32>,
        #[serde(default)]
        expected_percent: Option<f64>,
        #[serde(default)]
        id: Option<String>,
    },
    ReleaseLease { lease_id: String },
    ListLeases,
    /// The launcher's pick (`slopbar-run`): the account to run a command on and the config dir
    /// that uses it. Leases the account for the life of process `pid` when it is not the CLI's
    /// current sign-in (its project profile must stay signed in) or when `purpose` is given.
    PickAccount {
        provider: Provider,
        pid: u32,
        #[serde(default)]
        purpose: Option<String>,
        #[serde(default)]
        expected_percent: Option<f64>,
    },
    /// Claude Code's SessionStart hook (see `session_hook`): record which account the session's
    /// config dir (None: the default login) is signed in to.
    RecordSession {
        session_id: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        config_dir: Option<String>,
    },
    /// Claude Code's turn hook (see `turn_hook`): a session started ("prompt") or ended ("stop") a
    /// turn. The reply's `context` or `block` is what the hook prints (see `wind_down`).
    Turn {
        session_id: String,
        event: String,
        #[serde(default)]
        config_dir: Option<String>,
    },
    SwitchAccount { provider: Provider, id: String },
    AddAccount { provider: Provider },
    ConnectMonitoring { provider: Provider, id: String },
    DisconnectMonitoring { provider: Provider, id: String },
    RemoveAccount { provider: Provider, id: String },
    /// Revoke every sign-in SlopBar holds for the account (see `app::revoke_sessions`). Without
    /// `confirm` only says what that would do.
    RevokeAccountSessions {
        provider: Provider,
        id: String,
        #[serde(default)]
        confirm: bool,
        #[serde(default)]
        remove_slot: bool,
    },
    /// Monitoring sign-ins for every account without one, one after another. Without `confirm`
    /// only says which accounts that would be.
    ConnectMonitoringAll {
        #[serde(default)]
        confirm: bool,
    },
    /// "Sign in all accounts" (see `bulk::sign_in_plan`): pending accounts signed in again and, with
    /// `monitoring` (default true), monitoring for each account without it, by email. Without
    /// `confirm` only says what it would open.
    SignInAll {
        #[serde(default)]
        confirm: bool,
        #[serde(default = "yes")]
        monitoring: bool,
        /// Stop a run still going first (Retry from another computer).
        #[serde(default)]
        retry: bool,
    },
    /// Remove every account whose plan has ended (see `bulk::lapsed`). Without `confirm` only
    /// says which accounts that would be.
    RemoveLapsedAccounts {
        #[serde(default)]
        confirm: bool,
    },
    /// Folders pinned to an account, with their state and the lines that use them (see `pins`).
    ListProjectPins,
    /// Pin `folder` to the provider's account `id`, or remove that pin when `id` is null.
    SetProjectPin {
        folder: String,
        provider: Provider,
        #[serde(default)]
        id: Option<String>,
    },
    CheckForUpdates,
    InstallUpdate,
    GetAppInfo,
    OpenSettings,
    /// The visible text of a SlopBar window and a PNG of it, drawn off-screen when it is closed.
    InspectWindow {
        window: InspectTarget,
        #[serde(default)]
        page: Option<String>,
        #[serde(default = "yes")]
        screenshot: bool,
    },
    /// The self-check Settings > Health shows (see `health`).
    GetHealth,
    /// The account roster (see `roster`): whether it is on, what this machine last wrote and
    /// read back, the other machines' rosters, and the accounts waiting to be signed in here.
    /// With `sync`, writes and reads the rosters first.
    GetRoster {
        #[serde(default)]
        sync: bool,
    },
    /// Whether sign-ins can run here (see `pairing::assess`): build, desktop session, browser,
    /// roster and the accounts waiting. With `wait_seconds`, first waits (up to 45 s) for "Sign in
    /// all accounts" to change from `since` (the `sign_in_revision` of an earlier answer).
    GetReadiness {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_seconds: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
    },
    /// `get_roster` with `sync`, for the MCP tool `sync_roster`.
    SyncRoster,
    /// Stops a running "Sign in all accounts" or "Connect monitoring for all", as Settings' Stop does.
    StopSignIns,
    /// Saves the roster passphrase on this machine (null deletes it), then syncs with it.
    SetRosterPassphrase { passphrase: Option<crate::backup::Passphrase> },
    /// Where in-use status can be shared: detected sync folders and rclone remotes.
    ListSharingLocations,
    /// Open the provider's billing page in the user's browser (fixed URL per provider).
    OpenBillingPage { provider: Provider },
    /// Write the plan-renewal calendar (see `calendar`) to its path in settings now.
    ExportCalendar,
    /// Quit as the tray's Quit does.
    QuitApp,
    /// Quit, then start the installed executable again (picking up a new build).
    RestartApp,
}

/// How the door is doing, for Settings > Health: 0 starting, 1 listening, 2 retrying.
static STATE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
pub fn health() -> crate::health::Door {
    use crate::health::Door;
    if !cfg!(windows) {
        return Door::Unsupported;
    }
    match STATE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Door::Listening,
        2 => Door::Retrying,
        _ => Door::Starting,
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn set_state(state: u8) {
    STATE.store(state, std::sync::atomic::Ordering::Relaxed);
}

/// Settings pages `InspectWindow` can show (its sidebar's `data-page` values).
pub const SETTINGS_PAGES: [&str; 7] = ["accounts", "subscriptions", "usage", "notifications", "data", "general", "health"];

/// A window `InspectWindow` can look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectTarget {
    Settings,
    Flyout,
    /// The quick switcher (Ctrl+Alt+S by default).
    Switcher,
    /// The taskbar widget (the native one is drawn by the same code as on screen).
    Widget,
}

fn yes() -> bool { true }

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Reply {
    pub door: u32,
    pub result: Option<Value>,
    pub error: Option<String>,
}

/// The pipe for this user's app data: two Windows users never share one.
pub fn pipe_name() -> String {
    let dir = crate::settings::app_dir().map(|d| d.to_string_lossy().to_lowercase()).unwrap_or_default();
    // FNV-1a: stable across builds, unlike std's hasher.
    let hash = dir.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!(r"\\.\pipe\slopbar-door-{hash:016x}")
}

/// Where exports are written (see `diagnostics::Recorder::export`).
pub fn exports_dir() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("diagnostics").join("exports"))
}

/// Whether `path` names an export file directly inside `dir`, so an agent told to read an
/// export is never pointed at another file.
pub fn is_export(path: &Path, dir: &Path) -> bool {
    let name_ok = path.file_name().and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("SlopBar-Diagnostics-") && n.ends_with(".json") && !n.contains(".."));
    let parent_ok = path.parent().is_some_and(|p| p.as_os_str().eq_ignore_ascii_case(dir.as_os_str()));
    let file_ok = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file());
    name_ok && parent_ok && file_ok
}

/// Remembers the last export so a burst of requests (an agent retrying, or looping) reuses it.
#[derive(Default)]
pub struct Reuse(std::sync::Mutex<Option<(std::time::Instant, PathBuf)>>);

impl Reuse {
    pub fn recent(&self) -> Option<PathBuf> {
        let last = self.0.lock().unwrap();
        last.as_ref().filter(|(at, path)| at.elapsed() < REUSE_WINDOW && path.is_file()).map(|(_, p)| p.clone())
    }
    pub fn remember(&self, path: &Path) {
        *self.0.lock().unwrap() = Some((std::time::Instant::now(), path.to_owned()));
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn export_value(path: &Path, reused: bool) -> Value {
    serde_json::json!({
        "path": path.to_string_lossy(),
        "bytes": std::fs::metadata(path).ok().map(|m| m.len()),
        "reused_recent_export": reused,
    })
}

/// Checks an export reply: its path must be an export file in the export folder.
fn vet_export(result: Value, dir: Option<&Path>) -> Result<Value, String> {
    let dir = dir.ok_or("no_config_directory")?;
    let path = result.get("path").and_then(Value::as_str).ok_or("bad_door_reply")?;
    if !is_export(Path::new(path), dir) {
        return Err("bad_door_reply".into());
    }
    Ok(result)
}

/// Sends `request` to the running app and returns its result.
pub fn call(name: &str, request: &Request) -> Result<Value, String> {
    call_within(name, request, CLIENT_TIMEOUT)
}

/// `call`, giving up after `limit` (connecting included): for callers that must stay quick, such
/// as the status line.
pub fn call_within(name: &str, request: &Request, limit: Duration) -> Result<Value, String> {
    let reply = request_raw(name, request, limit)?;
    if let Some(error) = reply.error {
        return Err(error);
    }
    let result = reply.result.ok_or("bad_door_reply")?;
    match request {
        Request::ExportDiagnostics => vet_export(result, exports_dir().as_deref()),
        _ => Ok(result),
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::future::Future;
    use std::io::{BufRead, Read, Write};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::HANDLE;

    /// Connections served at once; more wait for a slot. A connection holds its slot until its
    /// answer is written, or its client has given up (see `answer`).
    pub(super) const MAX_CONNECTIONS: usize = 16;
    /// Reading the request and writing the reply. The operation itself is not bounded here: a
    /// connection waits for it until its client gives up, and it carries on after (see `answer`).
    const IO_TIMEOUT: Duration = Duration::from_secs(5);
    const RETRY: Duration = if cfg!(test) { Duration::from_secs(1) } else { Duration::from_secs(15) };

    /// What the door needs to know about a process's token.
    struct Identity {
        /// The user SID as text (e.g. "S-1-5-21-…").
        user: String,
        /// Mandatory integrity RID: 0x2000 medium, 0x3000 high.
        integrity: u32,
        elevated: bool,
        app_container: bool,
    }

    /// Reads one token information class into an aligned buffer.
    ///
    /// SAFETY: `token` must be an open token handle with TOKEN_QUERY access.
    unsafe fn token_info(token: HANDLE, class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS) -> std::io::Result<Vec<u64>> {
        use windows_sys::Win32::Security::GetTokenInformation;
        let mut len = 0u32;
        GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        // Fixed-size classes (elevation, AppContainer) require exactly the length reported.
        if GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(buf)
    }

    fn identity(process: HANDLE) -> std::io::Result<Identity> {
        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
        use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
        use windows_sys::Win32::Security::{GetSidSubAuthority, GetSidSubAuthorityCount, TokenElevation, TokenIntegrityLevel,
            TokenIsAppContainer, TokenUser, TOKEN_ELEVATION, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER};
        use windows_sys::Win32::System::Threading::OpenProcessToken;
        // SAFETY: standard token queries. Buffers are u64-aligned and sized by the system, every
        // pointer is valid for its call, and each handle or allocation is released once.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let read = || -> std::io::Result<Identity> {
                let user = token_info(token, TokenUser)?;
                let sid = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
                let mut text = std::ptr::null_mut();
                if ConvertSidToStringSidW(sid, &mut text) == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let n = (0..).take_while(|&i| *text.add(i) != 0).count();
                let user = String::from_utf16_lossy(std::slice::from_raw_parts(text, n));
                LocalFree(text.cast());
                let label = token_info(token, TokenIntegrityLevel)?;
                let label_sid = (*(label.as_ptr() as *const TOKEN_MANDATORY_LABEL)).Label.Sid;
                let count = *GetSidSubAuthorityCount(label_sid) as u32;
                let integrity = if count == 0 { 0 } else { *GetSidSubAuthority(label_sid, count - 1) };
                let elevation = token_info(token, TokenElevation)?;
                let elevated = (*(elevation.as_ptr() as *const TOKEN_ELEVATION)).TokenIsElevated != 0;
                let container = token_info(token, TokenIsAppContainer)?;
                let app_container = *(container.as_ptr() as *const u32) != 0;
                Ok(Identity { user, integrity, elevated, app_container })
            };
            let result = read();
            CloseHandle(token);
            result
        }
    }

    fn own_identity() -> std::io::Result<Identity> {
        // SAFETY: the pseudo-handle of the current process needs no closing.
        identity(unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() })
    }

    /// Whether the process serving this pipe is trustworthy: the same Windows user as this one,
    /// at least medium integrity and not sandboxed. Another user's process, or a low-integrity or
    /// AppContainer process of this user, that took the pipe name fails here.
    fn served_by_same_user(pipe: &std::fs::File) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        const MEDIUM: u32 = 0x2000;
        let mut pid = 0u32;
        // SAFETY: `pipe` is an open pipe handle; the process handle is closed once below.
        unsafe {
            if GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) == 0 {
                return false;
            }
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return false;
            }
            let theirs = identity(process);
            CloseHandle(process);
            match (theirs, own_identity()) {
                (Ok(server), Ok(me)) => server.user == me.user && server.integrity >= MEDIUM && !server.app_container,
                _ => false,
            }
        }
    }

    /// Owned by and open to the signed-in user (named explicitly) and SYSTEM; protected from
    /// inherited ACEs. When the app runs elevated, only elevated clients may connect.
    fn create(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        let me = own_identity()?;
        let sid = &me.user;
        // An elevated app only admits elevated clients: otherwise any ordinary process of the user
        // could have it start sign-ins or installs with admin rights.
        let label = if me.elevated { "S:(ML;;NWNR;;;HI)" } else { "" };
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY){label}").encode_utf16().chain([0]).collect();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and outlives the call; the descriptor it allocates is
        // freed below after the pipe has copied it.
        let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut descriptor, std::ptr::null_mut()) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: `attributes` points at a valid descriptor for the duration of the call.
        let server = unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(name, &mut attributes as *mut _ as *mut _)
        };
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW above.
        unsafe { LocalFree(descriptor.cast()) };
        server
    }

    /// Serves door requests until the process exits, reopening the door after any failure
    /// (including another process holding the name, e.g. a previous instance still exiting).
    /// `handle` performs one operation; exports it performs are reused for `REUSE_WINDOW`.
    pub async fn serve<F, Fut>(name: String, handle: F)
    where
        F: Fn(Request) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        serve_within(name, CLIENT_TIMEOUT, handle).await
    }

    /// `serve`, letting a connection go once its operation has run `limit` (see `answer`).
    pub(super) async fn serve_within<F, Fut>(name: String, limit: Duration, handle: F)
    where
        F: Fn(Request) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        let reuse = Arc::new(Reuse::default());
        loop {
            if let Err(e) = listen(&name, limit, handle.clone(), reuse.clone()).await {
                set_state(2);
                eprintln!("door closed, retrying: {e}");
            }
            tokio::time::sleep(RETRY).await;
        }
    }

    async fn listen<F, Fut>(name: &str, limit: Duration, handle: F, reuse: Arc<Reuse>) -> std::io::Result<()>
    where
        F: Fn(Request) -> Fut + Clone + Send + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        // Exports are serialized so a burst reuses the first one.
        let exporting = Arc::new(tokio::sync::Mutex::new(()));
        let mut server = create(name, true)?;
        set_state(1);
        loop {
            let slot = slots.clone().acquire_owned().await.expect("never closed");
            server.connect().await?;
            let connected = std::mem::replace(&mut server, create(name, false)?);
            let (handle, reuse, exporting) = (handle.clone(), reuse.clone(), exporting.clone());
            tokio::spawn(async move {
                let _ = answer(connected, limit, handle, reuse, exporting).await;
                drop(slot);
            });
        }
    }

    /// Reads one request, performs it and writes the answer. The operation runs in a task of its
    /// own: once it has run `limit` (CLIENT_TIMEOUT, so its client has given up by then) the door
    /// answers "timeout" and lets the connection, and its slot, go, while the operation carries on.
    /// So operations that wait long (for a refresh, a sign-in, a stuck folder) never use up the
    /// slots that get_app_info, restart_app, install_update and quit_app need.
    async fn answer<F, Fut>(pipe: NamedPipeServer, limit: Duration, handle: F, reuse: Arc<Reuse>, exporting: Arc<tokio::sync::Mutex<()>>) -> std::io::Result<()>
    where
        F: Fn(Request) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        let mut limited = (&mut reader).take(MAX_REQUEST);
        let read = limited.read_line(&mut line);
        tokio::time::timeout(IO_TIMEOUT, read).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
        let request = serde_json::from_str::<Request>(line.trim());
        let operation = tokio::spawn(async move {
            match request {
                Ok(Request::ExportDiagnostics) => {
                    let _one = exporting.lock().await;
                    match reuse.recent() {
                        Some(path) => Ok(export_value(&path, true)),
                        None => handle(Request::ExportDiagnostics).await.and_then(|result| {
                            let path = PathBuf::from(result.get("path").and_then(Value::as_str).ok_or("diagnostic_storage")?);
                            reuse.remember(&path);
                            Ok(export_value(&path, false))
                        }),
                    }
                }
                Ok(request) => handle(request).await,
                Err(_) => Err("unsupported_request".into()),
            }
        });
        let result = match tokio::time::timeout(limit, operation).await {
            Ok(Ok(result)) => result,
            // It panicked: no answer, so the client reads a closed pipe, as it always has.
            Ok(Err(_)) => return Ok(()),
            // Its client has given up by now; the operation carries on in its own task.
            Err(_) => Err("timeout".into()),
        };
        let reply = match result {
            Ok(result) => Reply { door: VERSION, result: Some(result), error: None },
            Err(error) => Reply { door: VERSION, result: None, error: Some(error) },
        };
        let mut bytes = serde_json::to_vec(&reply).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        let mut pipe = reader.into_inner();
        let write = async { pipe.write_all(&bytes).await?; pipe.flush().await };
        tokio::time::timeout(IO_TIMEOUT, write).await.map_err(|_| std::io::ErrorKind::TimedOut)?
    }

    /// Sends one request to the running app and waits (bounded) for its raw reply.
    pub fn request_raw(name: &str, request: &Request, limit: Duration) -> Result<Reply, String> {
        let deadline = std::time::Instant::now() + limit;
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;
        use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
        let mut tries = 0;
        let pipe = loop {
            // Identification only: whoever serves the pipe can never act as this process.
            match std::fs::OpenOptions::new().read(true).write(true).security_qos_flags(SECURITY_IDENTIFICATION).open(name) {
                Ok(pipe) => break pipe,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) && tries < 100 => {
                    tries += 1;
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err("timeout".into());
                    }
                    std::thread::sleep(left.min(Duration::from_millis(100)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("not_running".into()),
                Err(_) => return Err("door_unavailable".into()),
            }
        };
        if !served_by_same_user(&pipe) {
            return Err("door_untrusted".into());
        }
        let mut line = serde_json::to_vec(request).map_err(|_| "door_unavailable")?;
        line.push(b'\n');
        (&pipe).write_all(&line).map_err(|_| "door_unavailable")?;
        // A stuck server must not hang the caller: read on a worker thread, bounded in time and
        // size. On timeout the worker ends when the pipe closes with this process.
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut answer = Vec::new();
            let result = std::io::BufReader::new((&pipe).take(MAX_REPLY)).read_until(b'\n', &mut answer).map(|_| answer);
            let _ = send.send(result);
        });
        let answer = match receive.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => return Err("door_unavailable".into()),
            Err(_) => return Err("timeout".into()),
        };
        parse_reply(&answer)
    }
}

/// Reads the app's reply line. A pipe closed without one is an app quitting or starting (as
/// during a restart onto a new build), so it is `door_unavailable`, not an unusable answer.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_reply(answer: &[u8]) -> Result<Reply, String> {
    if answer.iter().all(u8::is_ascii_whitespace) {
        return Err("door_unavailable".into());
    }
    serde_json::from_slice(answer).map_err(|_| "bad_door_reply".into())
}

/// Waits (up to `limit`) for process `pid` to exit; used by a restarted app so the single-instance
/// check never sees the old one still running.
#[cfg(windows)]
pub fn wait_for_exit(pid: u32, limit: Duration) {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    // SAFETY: the handle, if any, is waited on and closed once.
    unsafe {
        let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if process.is_null() {
            return; // Already gone.
        }
        let _ = WaitForSingleObject(process, limit.as_millis().min(u32::MAX as u128) as u32) == WAIT_OBJECT_0;
        CloseHandle(process);
    }
}

#[cfg(not(windows))]
pub fn wait_for_exit(_pid: u32, limit: Duration) {
    std::thread::sleep(limit.min(Duration::from_secs(2)));
}

#[cfg(windows)]
use imp::request_raw;
#[cfg(windows)]
#[cfg_attr(not(feature = "desktop"), allow(unused_imports))]
pub use imp::serve;

#[cfg(not(windows))]
fn request_raw(_name: &str, _request: &Request, _limit: Duration) -> Result<Reply, String> {
    Err("unsupported_platform".into())
}

#[cfg(test)]
mod vet_tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn only_export_files_in_the_export_folder_pass() {
        let dir = TempDir::new();
        let good = dir.0.join("SlopBar-Diagnostics-1.json");
        std::fs::write(&good, b"{}").unwrap();
        std::fs::write(dir.0.join("history.bin"), b"x").unwrap();
        let vet = |path: &Path| vet_export(serde_json::json!({ "path": path.to_string_lossy() }), Some(&dir.0));
        assert!(vet(&good).is_ok());
        for bad in [dir.0.join("history.bin"), dir.0.join("SlopBar-Diagnostics-missing.json"),
                    dir.0.join("sub").join("SlopBar-Diagnostics-1.json"), PathBuf::from(r"C:\Windows\win.ini")] {
            assert_eq!(vet(&bad).unwrap_err(), "bad_door_reply", "{bad:?}");
        }
    }

    #[test]
    fn a_pipe_closed_without_a_reply_is_a_restart_not_a_bad_answer() {
        assert_eq!(parse_reply(b"").unwrap_err(), "door_unavailable");
        assert_eq!(parse_reply(b"\n").unwrap_err(), "door_unavailable");
        assert_eq!(parse_reply(b"{\"door\":1,\"res").unwrap_err(), "bad_door_reply");
        let reply = parse_reply(b"{\"door\":1,\"result\":{\"machine\":{\"id\":\"x\",\"name\":\"y\"}},\"error\":null}\n").unwrap();
        assert_eq!(reply.result.unwrap()["machine"]["name"], "y");
    }

    #[test]
    fn a_burst_reuses_the_last_export_only_briefly() {
        let dir = TempDir::new();
        let file = dir.0.join("SlopBar-Diagnostics-1.json");
        std::fs::write(&file, b"{}").unwrap();
        let reuse = Reuse::default();
        assert!(reuse.recent().is_none());
        reuse.remember(&file);
        assert_eq!(reuse.recent(), Some(file.clone()));
        *reuse.0.lock().unwrap() = Some((std::time::Instant::now() - REUSE_WINDOW, file.clone()));
        assert!(reuse.recent().is_none());
        reuse.remember(&file);
        std::fs::remove_file(&file).unwrap();
        assert!(reuse.recent().is_none(), "a deleted export is never offered");
    }

    #[test]
    fn requests_are_strict() {
        let parse = |s: &str| serde_json::from_str::<Request>(s);
        assert!(matches!(parse(r#"{"op":"switch_account","provider":"claude","id":"x"}"#), Ok(Request::SwitchAccount { .. })));
        assert!(parse(r#"{"op":"switch_account","provider":"gemini","id":"x"}"#).is_err());
        assert!(parse(r#"{"op":"remove_account","provider":"claude","id":"x","force":true}"#).is_err());
        assert!(parse(r#"{"op":"read_vault"}"#).is_err());
        assert!(matches!(parse(r#"{"op":"restore_backup","path":"C:\\b.slopbar-backup","passphrase":"correct horse"}"#), Ok(Request::RestoreBackup { dry_run: true, .. })), "restoring previews by default");
        let export = parse(r#"{"op":"export_backup","passphrase":"correct horse"}"#).unwrap();
        assert!(!format!("{export:?}").contains("correct horse"), "a passphrase never reaches debug output");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn test_pipe() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        format!(r"\\.\pipe\slopbar-door-test-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    async fn ask(name: String, line: &'static [u8]) -> Reply {
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let pipe = std::fs::OpenOptions::new().read(true).write(true).open(&name).unwrap();
            (&pipe).write_all(line).unwrap();
            let mut answer = String::new();
            std::io::BufReader::new(&pipe).read_line(&mut answer).unwrap();
            serde_json::from_str(answer.trim()).unwrap()
        }).await.unwrap()
    }

    async fn blocking_call(name: &str, request: Request) -> Result<Reply, String> {
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || imp::request_raw(&name, &request, CLIENT_TIMEOUT)).await.unwrap()
    }

    /// Serves a fake app: exports `file`, echoes every other operation, refuses removals.
    fn fake_app(name: &str, file: PathBuf) -> std::sync::Arc<std::sync::atomic::AtomicU32> {
        let exports = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counted = exports.clone();
        tokio::spawn(serve(name.to_owned(), move |request| {
            let (counted, file) = (counted.clone(), file.clone());
            async move {
                match request {
                    Request::ExportDiagnostics => {
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(serde_json::json!({ "path": file.to_string_lossy() }))
                    }
                    Request::RemoveAccount { .. } => Err("active".to_string()),
                    other => Ok(serde_json::to_value(&other).unwrap()),
                }
            }
        }));
        exports
    }

    fn export_file() -> (crate::test_support::TempDir, PathBuf) {
        let dir = crate::test_support::TempDir::new();
        let file = dir.0.join("SlopBar-Diagnostics-1.json");
        std::fs::write(&file, b"{}").unwrap();
        (dir, file)
    }

    #[tokio::test]
    async fn operations_reach_the_app_and_exports_are_reused_within_a_burst() {
        let name = test_pipe();
        let (_dir, file) = export_file();
        let exports = fake_app(&name, file.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;

        for reused in [false, true] {
            let reply = blocking_call(&name, Request::ExportDiagnostics).await.unwrap();
            assert_eq!(reply.door, VERSION);
            let result = reply.result.unwrap();
            assert_eq!(result["path"], file.to_string_lossy().as_ref());
            assert_eq!(result["bytes"], 2);
            assert_eq!(result["reused_recent_export"], reused);
        }
        assert_eq!(exports.load(std::sync::atomic::Ordering::SeqCst), 1, "the second request reuses the first export");

        let switched = blocking_call(&name, Request::SwitchAccount { provider: Provider::Codex, id: "a".into() }).await.unwrap();
        assert_eq!(switched.result.unwrap(), serde_json::json!({ "op": "switch_account", "provider": "codex", "id": "a" }));
        let refused = blocking_call(&name, Request::RemoveAccount { provider: Provider::Claude, id: "a".into() }).await.unwrap();
        assert_eq!(refused.error.as_deref(), Some("active"));

        for line in [&b"{\"op\":\"read_vault\"}\n"[..], b"not json\n"] {
            let reply = ask(name.clone(), line).await;
            assert_eq!(reply.error.as_deref(), Some("unsupported_request"));
            assert!(reply.result.is_none());
        }
    }

    #[tokio::test]
    async fn silent_clients_time_out_without_blocking_others() {
        let name = test_pipe();
        let (_dir, file) = export_file();
        fake_app(&name, file);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Every slot plus the waiting instance taken by clients that never send anything.
        let n = name.clone();
        let idle = tokio::task::spawn_blocking(move || (0..17).map(|_| loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(&n) {
                Ok(pipe) => break pipe,
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }).collect::<Vec<_>>()).await.unwrap();
        let started = std::time::Instant::now();
        let reply = blocking_call(&name, Request::GetUsage).await.unwrap();
        assert!(reply.result.is_some());
        assert!(started.elapsed() < Duration::from_secs(20), "idle clients are dropped after the I/O timeout");
        drop(idle);
    }

    #[tokio::test]
    async fn the_door_reopens_once_a_squatter_leaves() {
        let name = test_pipe();
        let squatter = tokio::net::windows::named_pipe::ServerOptions::new().first_pipe_instance(true).create(&name).unwrap();
        let (_dir, file) = export_file();
        fake_app(&name, file);
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(squatter);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let reply = blocking_call(&name, Request::GetAppInfo).await.unwrap();
        assert!(reply.result.is_some());
    }

    #[test]
    fn reports_when_the_app_is_not_running() {
        assert_eq!(call(&test_pipe(), &Request::GetUsage).unwrap_err(), "not_running");
    }

    use crate::refresh_wait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;

    /// How long a test waits for what should happen at once (an answer, a door listening). The wait
    /// ends as soon as it happens, so only a starved run waits at all, and none this long.
    const PATIENT: Duration = Duration::from_secs(5 * 60);
    /// What the tests stall on ends by itself after this. A limit in the code ends a wait far
    /// sooner, however starved the run (tokio fires timers in deadline order); with the limit gone,
    /// the wait ends here and the test fails, rather than hanging.
    const SELF_ENDING: Duration = Duration::from_secs(60);
    /// The limits the tests give the code: far inside SELF_ENDING.
    const SHORT: Duration = Duration::from_millis(100);
    /// A last backstop for each test.
    const TEST_LIMIT: Duration = Duration::from_secs(5 * 60);

    async fn within_test_limit<T>(test: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(TEST_LIMIT, test).await.expect("the test ran past its limit: a limit in the code is missing")
    }

    /// Waits up to PATIENT for `check` to hold, without racing the task that makes it so.
    async fn eventually(what: &str, check: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + PATIENT;
        while !check() {
            assert!(std::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Marks a test over once it is dropped (passed, failed or past its limit), so its clients stop
    /// asking a door that is gone.
    struct Over(Arc<AtomicBool>);

    impl Over {
        fn new() -> Self {
            Over(Arc::new(AtomicBool::new(false)))
        }
    }

    impl Drop for Over {
        fn drop(&mut self) {
            self.0.store(true, SeqCst);
        }
    }

    /// One request, as a client that waits up to PATIENT, through a door that is busy or not open
    /// yet, as a starved run may find it; until the test is `over`. On a thread of its own, which
    /// the runtime does not wait for as a failed test ends: the runtime ends without closing a pipe
    /// instance whose connect was pending, so a client can be left waiting there for PATIENT.
    fn ask_patiently(over: &Over, name: &str, request: Request) -> tokio::sync::oneshot::Receiver<Reply> {
        let (name, over) = (name.to_owned(), over.0.clone());
        let (send, answer) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + PATIENT;
            let reply = loop {
                match imp::request_raw(&name, &request, PATIENT) {
                    Ok(reply) => break reply,
                    Err(e) if over.load(SeqCst) || std::time::Instant::now() >= deadline => {
                        break Reply { door: VERSION, result: None, error: Some(format!("no answer: {e}")) };
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            };
            let _ = send.send(reply);
        });
        answer
    }

    /// A fake app served within `limit`, whose refresh is `refresh` and which echoes the rest.
    fn fake_app_within<R, RF>(name: &str, limit: Duration, refresh: R)
    where
        R: Fn() -> RF + Clone + Send + Sync + 'static,
        RF: std::future::Future<Output = Result<Value, String>> + Send + 'static,
    {
        tokio::spawn(imp::serve_within(name.to_owned(), limit, move |request| {
            let refresh = refresh.clone();
            async move {
                match request {
                    Request::Refresh => refresh().await,
                    other => Ok(serde_json::to_value(&other).unwrap()),
                }
            }
        }));
    }

    /// Takes `lock` now and keeps it until SELF_ENDING, as an update's handover that never returns
    /// keeps the refresh lock. Returns a flag set once it lets go.
    fn hold_for_good(lock: &Arc<tokio::sync::Mutex<()>>) -> Arc<AtomicBool> {
        let held = lock.clone().try_lock_owned().expect("a free lock");
        let let_go = Arc::new(AtomicBool::new(false));
        let flag = let_go.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SELF_ENDING).await;
            drop(held);
            flag.store(true, SeqCst);
        });
        let_go
    }

    /// Every slot taken by a request whose operation is stuck (a refresh on a folder that never
    /// answers, say): each connection goes once its client has given up, so get_app_info answers.
    #[tokio::test]
    async fn a_request_still_running_when_its_client_gives_up_frees_its_slot() {
        within_test_limit(async {
            let over = Over::new();
            let name = test_pipe();
            let (started, ended) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
            let (s, e) = (started.clone(), ended.clone());
            fake_app_within(&name, SHORT, move || {
                let (s, e) = (s.clone(), e.clone());
                async move {
                    s.fetch_add(1, SeqCst);
                    tokio::time::sleep(SELF_ENDING).await;
                    e.store(true, SeqCst);
                    Ok(serde_json::json!({ "accounts": [] }))
                }
            });
            ask_patiently(&over, &name, Request::GetAppInfo).await.unwrap();
            let stuck: Vec<_> = (0..imp::MAX_CONNECTIONS).map(|_| ask_patiently(&over, &name, Request::Refresh)).collect();
            eventually("every slot holds a stuck operation", || started.load(SeqCst) >= imp::MAX_CONNECTIONS).await;
            let info = ask_patiently(&over, &name, Request::GetAppInfo).await.unwrap();
            assert!(!ended.load(SeqCst), "get_app_info was answered only once a stuck operation ended and gave its slot up");
            assert_eq!(info.result.unwrap()["op"], "get_app_info");
            for reply in stuck {
                assert_eq!(reply.await.unwrap().error.as_deref(), Some("timeout"), "its client hears it is still running");
            }
        })
        .await
    }

    /// The refresh lock held for good (an update's handover that never returns) and more refreshes
    /// asked for than the door has slots: each answers, saying it gave up, and get_app_info,
    /// install_update, restart_app and quit_app answer while the lock is still held. The door's own
    /// limit is out of reach here: the refreshes' limit alone keeps the slots free.
    #[tokio::test]
    async fn refreshes_waiting_on_a_lock_held_for_good_answer_and_the_door_keeps_answering() {
        within_test_limit(async {
            let over = Over::new();
            let name = test_pipe();
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            let let_go = hold_for_good(&lock);
            let started = Arc::new(AtomicUsize::new(0));
            let (l, s) = (lock.clone(), started.clone());
            // The door's refresh as the app answers it (`app::door_reply`, `app::refresh_within`).
            fake_app_within(&name, PATIENT, move || {
                let (l, s) = (l.clone(), s.clone());
                async move {
                    s.fetch_add(1, SeqCst);
                    let fresh = || async { serde_json::json!({ "accounts": [], "fresh": true }) };
                    refresh_wait::noted(async { Ok(refresh_wait::under(&l, SHORT, fresh).await.unwrap_or_else(|| serde_json::json!({ "accounts": [] }))) }).await
                }
            });
            ask_patiently(&over, &name, Request::GetAppInfo).await.unwrap();
            let refreshes: Vec<_> = (0..imp::MAX_CONNECTIONS + 4).map(|_| ask_patiently(&over, &name, Request::Refresh)).collect();
            eventually("the refreshes take every slot", || started.load(SeqCst) >= imp::MAX_CONNECTIONS).await;
            for request in [Request::GetAppInfo, Request::InstallUpdate, Request::RestartApp, Request::QuitApp] {
                let op = serde_json::to_value(&request).unwrap()["op"].clone();
                let reply = ask_patiently(&over, &name, request).await.unwrap();
                assert_eq!(reply.result.map(|r| r["op"].clone()), Some(op));
            }
            assert!(!let_go.load(SeqCst), "the door answered only once the lock was let go");
            for reply in refreshes {
                let result = reply.await.unwrap().result.expect("each refresh answers");
                assert_eq!(result[refresh_wait::WAITING], refresh_wait::WAITING_NOTE, "and says it gave up: {result}");
                assert!(result.get("fresh").is_none(), "{result}");
            }
        })
        .await
    }

    /// An operation whose client gave up carries on: the door only lets the connection go, as the
    /// "timeout" it answers says (mcp's explain: "it keeps going").
    #[tokio::test]
    async fn an_operation_whose_client_gave_up_carries_on() {
        within_test_limit(async {
            let over = Over::new();
            let name = test_pipe();
            let finished = Arc::new(AtomicBool::new(false));
            let f = finished.clone();
            fake_app_within(&name, SHORT, move || {
                let f = f.clone();
                async move {
                    tokio::time::sleep(SHORT * 3).await;
                    f.store(true, SeqCst);
                    Ok(serde_json::json!({ "accounts": [] }))
                }
            });
            let reply = ask_patiently(&over, &name, Request::Refresh).await.unwrap();
            assert_eq!(reply.error.as_deref(), Some("timeout"), "its client hears it is still running");
            eventually("the operation carried on after its client gave up", || finished.load(SeqCst)).await;
        })
        .await
    }

    /// The door the app opens waits for an operation as long as its client does (CLIENT_TIMEOUT):
    /// never less, which would answer "timeout" to a client still waiting for the answer.
    #[test]
    fn the_door_the_app_opens_waits_as_long_as_its_client() {
        let source = include_str!("door.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(production.contains("serve_within(name, CLIENT_TIMEOUT, handle).await"), "serve passes the client's own limit");
        assert_eq!(production.matches("serve_within(").count(), 1, "the app's door is opened that way only");
    }

    /// An operation that panics leaves its client a closed pipe, as it always has: never an answer
    /// that it is still running.
    #[tokio::test]
    async fn an_operation_that_panics_leaves_its_client_a_closed_pipe() {
        within_test_limit(async {
            let over = Over::new();
            let name = test_pipe();
            tokio::spawn(imp::serve_within(name.clone(), PATIENT, |request| async move {
                match request {
                    Request::GetSettings => panic!("synthetic panic"),
                    other => Ok(serde_json::to_value(&other).unwrap()),
                }
            }));
            ask_patiently(&over, &name, Request::GetAppInfo).await.unwrap();
            let n = name.clone();
            let answer = tokio::task::spawn_blocking(move || imp::request_raw(&n, &Request::GetSettings, PATIENT)).await.unwrap();
            assert_eq!(answer.err().as_deref(), Some("door_unavailable"), "a closed pipe, not an answer");
            assert!(ask_patiently(&over, &name, Request::GetAppInfo).await.unwrap().result.is_some(), "and the door goes on");
        })
        .await
    }
}
