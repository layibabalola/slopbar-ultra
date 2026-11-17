//! Connecting this computer to the user's shared location in one go (Settings > Data > Other
//! computers, or an agent through the door): get rclone if it is missing, have rclone connect
//! Dropbox, turn in-use sharing on, and say which other computers share there.
//!
//! rclone does the Dropbox sign-in itself (`rclone config create`: its own local web server and
//! its own config file), so SlopBar never reads or passes the token, as `rclone` promises. rclone
//! never opens a browser (`config_auth_no_browser`): SlopBar reads the local link it waits on from
//! its notices and opens that in the browser, here or, through an SSH tunnel, on the computer
//! whose browser finishes the sign-in. Its standard output, which ends with the token, stays shut.

/// The rclone release SlopBar downloads when none is installed. It is fixed, with each zip's
/// SHA-256 as rclone publishes it (SHA256SUMS) and as winget's reviewed manifest records it.
pub const RCLONE_VERSION: &str = "v1.76.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    /// `std::env::consts::ARCH`.
    pub arch: &'static str,
    pub file: &'static str,
    pub sha256: &'static str,
}

const ASSETS: [Asset; 2] = [
    Asset { arch: "x86_64", file: "rclone-v1.76.0-windows-amd64.zip", sha256: "8b4fd2737ade4e70c057ef4a49deb7cffe94b0fb9d7587c189d149823f21a1dc" },
    Asset { arch: "aarch64", file: "rclone-v1.76.0-windows-arm64.zip", sha256: "c406c8a69a5a408e50624aa990fbb0a6266b3a4e83782a94de3cb96f50f86f10" },
];

/// The zip for this processor, if rclone publishes one SlopBar knows.
pub fn asset(arch: &str) -> Option<Asset> {
    ASSETS.iter().copied().find(|a| a.arch == arch)
}

/// Where the zip is published: rclone's own site, then its GitHub release (the same file).
pub fn urls(asset: &Asset) -> [String; 2] {
    [
        format!("https://downloads.rclone.org/{RCLONE_VERSION}/{}", asset.file),
        format!("https://github.com/rclone/rclone/releases/download/{RCLONE_VERSION}/{}", asset.file),
    ]
}

/// Whether the download may go to `host`: rclone's own site, GitHub, whose release links answer
/// with a redirect, and GitHub's hosts for release files (`release-assets.githubusercontent.com`,
/// `objects.githubusercontent.com` before it). The zip's SHA-256 is checked whichever serves it.
pub fn download_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    host == "downloads.rclone.org" || host == "github.com" || host.ends_with(".githubusercontent.com")
}

/// Whether a redirect may take the download to `url`: HTTPS on its standard port, no user name or
/// password, and a host `download_host` names (never an address).
pub fn download_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && url.username().is_empty() && url.password().is_none() && url.port().is_none()
        && url.host_str().is_some_and(download_host)
}

/// The most redirects one source's download follows (GitHub's takes one).
const MAX_REDIRECTS: usize = 5;

/// The client that downloads rclone: SlopBar's own (`net::builder`, which trusts what Windows
/// trusts), following a redirect only over HTTPS to a link `allowed` accepts, and at most
/// MAX_REDIRECTS. Any other redirect is not followed: its answer comes back as it is, for `fetch`
/// to say where it led. No Referer goes along. SlopBar's other requests follow no redirect at all.
pub fn download_client_with(builder: reqwest::ClientBuilder, allowed: fn(&reqwest::Url) -> bool) -> reqwest::Client {
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        let next = attempt.url();
        if attempt.previous().len() <= MAX_REDIRECTS && next.scheme() == "https" && allowed(next) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    builder.redirect(policy).referer(false).build().expect("download client")
}

/// The one client rclone's download uses (`install_rclone` makes it itself).
pub fn download_client() -> reqwest::Client {
    download_client_with(crate::net::builder(), download_url)
}

/// Why one source's download failed, as Settings and agents see it: the source's host (always one
/// `download_host` allows) and never a link, whose query may carry a signature.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Reason {
    pub host: String,
    pub why: crate::net::Why,
    /// The HTTP status, when an answer came.
    pub status: Option<u16>,
    /// Where a redirect that was not followed led: a host (with `http://` when it was not HTTPS).
    pub to: Option<String>,
}

/// The zip is about 25 MB; anything far larger is not it.
const MAX_ZIP: u64 = 128 * 1024 * 1024;

/// A download that receives nothing for this long has stalled.
const STALL: std::time::Duration = std::time::Duration::from_secs(30);

/// The most one source's download may take, however it trickles: every step of a setup ends, since
/// an automatic update waits while one runs. Cancel ends it sooner.
const DOWNLOAD_LIMIT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The most unpacking may take (tar.exe, or PowerShell's Expand-Archive), and checking the
/// unpacked rclone's version.
const UNPACK_LIMIT: std::time::Duration = std::time::Duration::from_secs(3 * 60);
const VERSION_LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether a program could not start because Windows or security software refused it: antivirus
/// that found it infected or removed it (225, 226), a software restriction or AppLocker policy
/// (786, 1260), a compatibility block (1259), Application Control and Smart App Control
/// (4551-4559, 4580-4582), or access denied. SlopBar then stops and says so; it never tries another
/// way to run it, nor another copy.
pub fn refused_to_start(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(225 | 226 | 786 | 1259 | 1260 | 4551..=4559 | 4580..=4582))
        || error.kind() == std::io::ErrorKind::PermissionDenied
}

/// What the setup does about rclone.
#[derive(Debug, PartialEq, Eq)]
pub enum RcloneStep {
    /// Use the rclone found.
    Use(std::path::PathBuf),
    /// Download SlopBar's own copy: none was found, the one found is too old, or SlopBar's own copy
    /// is broken.
    Download,
    /// Stop with this error code.
    Stop(&'static str),
}

/// What to do with what `rclone::find` found (and `rclone::check`'s verdict on it), `own` being
/// SlopBar's own copy's path. An rclone that Windows or security software will not run ends the
/// setup (`rclone_blocked`), and so does an installed one that fails (`rclone_broken`): SlopBar
/// never puts a copy of its own, somewhere else, in place of one that a policy stopped.
pub fn rclone_step(found: Option<(std::path::PathBuf, Result<(), crate::rclone::Unusable>)>, own: Option<&std::path::Path>) -> RcloneStep {
    use crate::rclone::Unusable;
    match found {
        None => RcloneStep::Download,
        Some((path, Ok(()))) => RcloneStep::Use(path),
        Some((_, Err(Unusable::Refused))) => RcloneStep::Stop("rclone_blocked"),
        Some((_, Err(Unusable::Old))) => RcloneStep::Download,
        Some((path, Err(Unusable::Failed))) if own.is_some_and(|own| own == path) => RcloneStep::Download,
        Some((_, Err(Unusable::Failed))) => RcloneStep::Stop("rclone_broken"),
    }
}

/// Runs a helper to its end within `limit`, its standard output collected, and kills it after: a
/// helper that hangs must not keep the setup (and the automatic update waiting on it) forever.
/// None when it could not start (with why) or was stopped at its limit.
fn run_within(mut command: std::process::Command, limit: std::time::Duration) -> Result<std::process::Output, Option<std::io::Error>> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(Some)?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(None);
    };
    let (send, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = send.send(out);
    });
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            // Its output ends with it; one still held open elsewhere is not waited for.
            Ok(Some(status)) => {
                let stdout = read.recv_timeout(std::time::Duration::from_secs(5)).unwrap_or_default();
                return Ok(std::process::Output { status, stdout, stderr: Vec::new() });
            }
            Ok(None) if started.elapsed() < limit => std::thread::sleep(std::time::Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(None);
            }
        }
    }
}

/// The oldest installed rclone the setup uses (`config create` with `config_` settings and
/// `--no-output`); with an older one, SlopBar gets its own copy.
const MIN_MINOR: u32 = 60;

/// Whether `rclone version`'s first line names v1.60 or later.
pub fn recent_enough(version: &str) -> bool {
    let Some(rest) = version.trim_start().strip_prefix("rclone v") else { return false };
    let mut parts = rest.split(|c: char| !c.is_ascii_digit()).map(|p| p.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), _) if major > 1 => true,
        (Some(1), Some(minor)) => minor >= MIN_MINOR,
        _ => false,
    }
}

/// How long the user has to allow access on Dropbox's page.
pub const SIGN_IN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// The remote name the setup uses unless told otherwise: what the user's other computers use.
pub const DEFAULT_REMOTE: &str = "dropbox";

/// Where a setup stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    /// Finding rclone, or downloading it.
    Rclone,
    /// rclone's Dropbox sign-in: waiting for the user to allow access.
    Dropbox,
    /// Turning sharing on and reading who else shares there.
    Sharing,
    Done,
    Failed,
}

impl Phase {
    pub fn running(self) -> bool {
        matches!(self, Phase::Rclone | Phase::Dropbox | Phase::Sharing)
    }
}

/// A setup's state, as Settings and agents see it. rclone's waiting link is kept apart (`link`
/// is never serialized): only `slopbar-mcp --sharing-state` hands its state value on.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Setup {
    pub phase: Phase,
    /// The rclone remote being set up.
    pub remote: String,
    /// rclone was downloaded by this setup.
    pub downloaded: bool,
    /// How much of rclone's download has arrived, in percent, while it downloads.
    pub progress: Option<u8>,
    /// rclone is waiting for the browser.
    pub waiting: bool,
    /// The sign-in is finished in a browser on another computer.
    pub via_tunnel: bool,
    #[serde(skip)]
    pub link: Option<String>,
    /// Why it stopped: an error code.
    pub error: Option<String>,
    /// The step it stopped at.
    pub failed_at: Option<Phase>,
    /// When rclone's download failed: why, for each source tried (`Reason`: hosts, never links).
    pub reasons: Vec<Reason>,
    /// The other computers sharing there, by name, once sharing is on.
    pub found: Vec<String>,
    /// Bumped on every change.
    pub revision: u64,
}

/// An SSH tunnel for rclone's port, while a sign-in for another computer waits: both loopback
/// addresses here forward to that computer's 127.0.0.1:53682 (Dropbox sends the browser to
/// `localhost`, which browsers try over IPv6 first). Ends with the app.
pub struct Tunnel {
    pub host: String,
    /// When it opened: a timer closing it later must not close a newer one.
    pub opened: std::time::Instant,
    child: std::process::Child,
    #[cfg(windows)]
    _tree: Option<crate::login::ProcessTree>,
}

impl Tunnel {
    /// Opens it, and fails when either address cannot be bound (another program holds the port)
    /// or SSH cannot connect. `ssh` is the system's OpenSSH client.
    pub fn open(ssh: &std::path::Path, host: &str) -> Result<Tunnel, &'static str> {
        use std::process::Stdio;
        if !crate::pairing::valid_host(host) {
            return Err("bad_ssh_host");
        }
        let mut command = std::process::Command::new(ssh);
        command.args(["-N", "-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=15",
                "-o", "ExitOnForwardFailure=yes",
                "-L", "127.0.0.1:53682:127.0.0.1:53682", "-L", "[::1]:53682:127.0.0.1:53682", host])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn().map_err(|_| "ssh_missing")?;
        #[cfg(windows)]
        let tree = crate::login::ProcessTree::attach(&child).ok();
        // A failed forward or connection ends ssh at once; a live tunnel keeps running.
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_secs(4) {
            if let Ok(Some(_)) = child.try_wait() {
                return Err("tunnel_failed");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Ok(Tunnel { host: host.to_string(), opened: std::time::Instant::now(), child, #[cfg(windows)] _tree: tree })
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// SlopBar's own copy of rclone, beside its own executables; used only when no rclone is
/// installed (see `rclone::find`).
pub fn own_rclone() -> Option<std::path::PathBuf> {
    dirs::data_local_dir().map(|d| d.join("SlopBar").join("rclone").join(if cfg!(windows) { "rclone.exe" } else { "rclone" }))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Downloads the pinned rclone zip (its own site, then its GitHub release) with `download_client`,
/// takes it only when its SHA-256 matches, and puts its `rclone.exe` at `dest`. `progress` hears
/// each new percent; `cancel` ends the download. Each source that fails adds its reason to
/// `reasons`. Error codes: `rclone_unsupported`, `download_failed`, `hash_mismatch`,
/// `extract_failed`, `rclone_blocked` (Windows or security software would not let it run),
/// `cancelled`.
pub async fn install_rclone(dest: &std::path::Path, cancel: &std::sync::atomic::AtomicBool, progress: &(dyn Fn(u8) + Send + Sync), reasons: &mut Vec<Reason>) -> Result<(), &'static str> {
    let asset = asset(std::env::consts::ARCH).ok_or("rclone_unsupported")?;
    install_from(&urls(&asset), asset, dest, cancel, progress, reasons).await
}

/// `install_rclone` from these sources.
async fn install_from(urls: &[String], asset: Asset, dest: &std::path::Path, cancel: &std::sync::atomic::AtomicBool, progress: &(dyn Fn(u8) + Send + Sync), reasons: &mut Vec<Reason>) -> Result<(), &'static str> {
    // Its own client, never one passed in: a client that followed no redirect once made GitHub's
    // link fail every time.
    let bytes = download(&download_client(), urls, asset.sha256, cancel, progress, reasons).await?;
    let dest = dest.to_path_buf();
    tokio::task::spawn_blocking(move || extract(&bytes, &asset, &dest)).await.unwrap_or(Err("extract_failed"))
}

/// The first of `urls` whose answer has the SHA-256 `sha256`; each that fails adds its reason. The
/// next source is tried only after one that was down or broken (`Why::blocks`): after a refusal
/// that may be on purpose, such as a filter's page, an untrusted certificate or a cut connection,
/// the download ends, so a company that blocks it from one site is not worked around through
/// another. Error codes: `hash_mismatch` when a zip came but none matched, `download_failed`,
/// `cancelled`.
async fn download(client: &reqwest::Client, urls: &[String], sha256: &str, cancel: &std::sync::atomic::AtomicBool, progress: &(dyn Fn(u8) + Send + Sync), reasons: &mut Vec<Reason>) -> Result<Vec<u8>, &'static str> {
    use crate::net::Why;
    let mut failure = "download_failed";
    for url in urls {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("cancelled");
        }
        let reason = match fetch(client, url, cancel, progress).await {
            Ok(bytes) if sha256_hex(&bytes) == sha256 => return Ok(bytes),
            // Not even a zip: a page or a message in its place, which is what a filter that
            // blocks the download may send with a 200.
            Ok(bytes) if !bytes.starts_with(b"PK\x03\x04") => Reason { host: host_of(url), why: Why::Page, status: Some(200), to: None },
            Ok(_) => {
                failure = "hash_mismatch";
                Reason { host: host_of(url), why: Why::Mismatch, status: Some(200), to: None }
            }
            Err(Fetch::Cancelled) => return Err("cancelled"),
            Err(Fetch::Failed(reason)) => reason,
        };
        let stop = reason.why.blocks(reason.status);
        reasons.push(reason);
        if stop {
            return Err("download_failed");
        }
    }
    Err(failure)
}

enum Fetch {
    Cancelled,
    Failed(Reason),
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).map(|u| crate::net::shown_host(&u)).unwrap_or_default()
}

async fn fetch(client: &reqwest::Client, url: &str, cancel: &std::sync::atomic::AtomicBool, progress: &(dyn Fn(u8) + Send + Sync)) -> Result<Vec<u8>, Fetch> {
    use crate::net::Why;
    // A failure names the source asked (its host), whichever hop of its redirects failed: an
    // error after a redirect carries the first link, not the hop's.
    let host = host_of(url);
    let failed = |why: Why, status: Option<u16>| Fetch::Failed(Reason { host: host.clone(), why, status, to: None });
    // The client's own limit is for small requests: this one may take minutes on a slow link, but
    // never more than DOWNLOAD_LIMIT in all (reqwest's limit covers the body too), and never STALL
    // without a byte.
    let send = client.get(url).timeout(DOWNLOAD_LIMIT).send();
    let mut response = match tokio::time::timeout(STALL, send).await {
        Err(_) => return Err(failed(Why::Timeout, None)),
        Ok(Err(e)) => return Err(failed(crate::net::why(&e), None)),
        Ok(Ok(response)) => response,
    };
    let status = response.status();
    if status.is_redirection() {
        // A redirect `download_client` does not follow: say where it led, never the link.
        let to = response.headers().get(reqwest::header::LOCATION).and_then(|l| l.to_str().ok())
            .and_then(|l| response.url().join(l).ok()).map(|u| crate::net::shown_host(&u));
        return Err(Fetch::Failed(Reason { host: host.clone(), why: Why::Redirected, status: Some(status.as_u16()), to }));
    }
    if status == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
        return Err(failed(Why::ProxyAuth, Some(407)));
    }
    // The site is down or the file is gone: its CDN says so with a page too, so this comes first,
    // and the next site may be tried (`Why::blocks`).
    if !Why::Status.blocks(Some(status.as_u16())) {
        return Err(failed(Why::Status, Some(status.as_u16())));
    }
    let html = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|t| t.to_str().ok())
        .is_some_and(|t| { let t = t.trim_start().to_ascii_lowercase(); t.starts_with("text/html") || t.starts_with("application/xhtml") });
    if html {
        // A page where the zip should be, with any other status: a filter's block page, typically.
        return Err(failed(Why::Page, Some(status.as_u16())));
    }
    if !status.is_success() {
        return Err(failed(Why::Status, Some(status.as_u16())));
    }
    if response.content_length().is_some_and(|n| n > MAX_ZIP) {
        return Err(failed(Why::TooLarge, Some(status.as_u16())));
    }
    let total = response.content_length().filter(|n| *n > 0);
    let (mut bytes, mut shown) = (Vec::new(), None);
    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Fetch::Cancelled);
        }
        let chunk = match tokio::time::timeout(STALL, response.chunk()).await {
            Err(_) => return Err(failed(Why::Timeout, Some(status.as_u16()))),
            Ok(Err(e)) => {
                // Cut short (the other end closed before the whole file came) is a broken
                // download; a reset or an alert mid-way may be a refusal.
                let why = match crate::net::why(&e) {
                    Why::Failed => Why::Interrupted,
                    _ if crate::net::cut_short(&e) => Why::Interrupted,
                    why => why,
                };
                return Err(failed(why, Some(status.as_u16())));
            }
            Ok(Ok(None)) => break,
            Ok(Ok(Some(chunk))) => chunk,
        };
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > MAX_ZIP {
            return Err(failed(Why::TooLarge, Some(status.as_u16())));
        }
        if let Some(total) = total {
            let percent = (bytes.len() as u64 * 100 / total).min(100) as u8;
            if shown != Some(percent) {
                shown = Some(percent);
                progress(percent);
            }
        }
    }
    Ok(bytes)
}

/// Unpacks only `rclone.exe` from the verified zip with Windows' own tar.exe (Windows 10 1803 and
/// later), or Windows PowerShell's Expand-Archive where tar is missing, then moves it into place
/// and checks that it runs and is the pinned version.
fn extract(zip: &[u8], asset: &Asset, dest: &std::path::Path) -> Result<(), &'static str> {
    let folder = dest.parent().ok_or("extract_failed")?;
    let work = folder.join(".unpack");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|_| "extract_failed")?;
    let result = (|| {
        let archive = work.join(asset.file);
        std::fs::write(&archive, zip).map_err(|_| "extract_failed")?;
        let inner = format!("{}/rclone.exe", asset.file.trim_end_matches(".zip"));
        let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
        let tar = system.join(r"System32\tar.exe");
        let unpack = if tar.is_file() {
            let mut tar = crate::sources::command(&tar.to_string_lossy());
            tar.arg("-xf").arg(&archive).arg("-C").arg(&work).arg(&inner);
            tar
        } else {
            powershell_unpack(&system, &archive, &work)
        };
        let unpacked = run_within(unpack, UNPACK_LIMIT).is_ok_and(|out| out.status.success());
        let exe = work.join(&inner);
        if !unpacked {
            return Err("extract_failed");
        }
        // Unpacked, then gone: antivirus took it.
        if !exe.is_file() {
            return Err("rclone_blocked");
        }
        let mut check = crate::sources::command(&exe.to_string_lossy());
        check.arg("version");
        let version = match run_within(check, VERSION_LIMIT) {
            Ok(version) => version,
            // Gone the moment after it was unpacked: antivirus took it.
            Err(Some(e)) if refused_to_start(&e) || e.kind() == std::io::ErrorKind::NotFound => return Err("rclone_blocked"),
            Err(_) => return Err("extract_failed"),
        };
        if !String::from_utf8_lossy(&version.stdout).starts_with(&format!("rclone {RCLONE_VERSION}")) {
            return Err("extract_failed");
        }
        let _ = std::fs::remove_file(dest);
        std::fs::rename(&exe, dest).map_err(|_| "extract_failed")
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// Windows PowerShell's Expand-Archive, unpacking `archive` into `work`, where tar.exe is missing;
/// any error ends it with a failure.
/// - The paths go in environment variables: after -Command, arguments become part of the script's
///   text, so `$args` would be empty and a path with a space would split.
/// - Windows' own Archive module is imported by its path, and PSModulePath is Windows PowerShell's
///   own module folder (to which it adds Program Files' one). Inherited from PowerShell 7 (SlopBar
///   or a test started from an agent's shell), PSModulePath put 7's Archive module first, whose
///   Expand-Archive fails in Windows PowerShell and still exited 0; an Archive module installed
///   for Windows PowerShell would stand in the same way.
/// - Expand-Archive stays (rather than .NET's ZipFile called from the script): Windows' own module
///   also runs where Constrained Language Mode (AppLocker, App Control) blocks .NET calls in scripts.
fn powershell_unpack(system: &std::path::Path, archive: &std::path::Path, work: &std::path::Path) -> std::process::Command {
    let home = system.join(r"System32\WindowsPowerShell\v1.0");
    let mut ps = crate::sources::command(&home.join("powershell.exe").to_string_lossy());
    ps.args(["-NoProfile", "-NonInteractive", "-Command",
        "$ErrorActionPreference = 'Stop'; Import-Module (Join-Path $PSHOME 'Modules\\Microsoft.PowerShell.Archive'); \
         Expand-Archive -LiteralPath $env:SLOPBAR_UNPACK_ZIP -DestinationPath $env:SLOPBAR_UNPACK_TO -Force"])
        .env("SLOPBAR_UNPACK_ZIP", archive).env("SLOPBAR_UNPACK_TO", work).env("PSModulePath", home.join("Modules"));
    ps
}

/// How rclone gets a Dropbox remote here: a new one, or a new sign-in for one that exists (its
/// access ended, or the user wants another Dropbox account).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    Create,
    Reconnect,
}

/// rclone's arguments for `how`: never a browser of its own, and nothing on standard output (it
/// ends with the remote, token included). `config update` of an existing remote signs in again.
pub fn connect_args(remote: &str, how: Connect) -> Vec<&str> {
    let mut args = match how {
        Connect::Create => vec!["config", "create", remote, "dropbox"],
        Connect::Reconnect => vec!["config", "update", remote, "config_refresh_token=true"],
    };
    args.extend(["config_auth_no_browser=true", "--no-output", "--ask-password=false"]);
    args
}

/// Runs rclone's own Dropbox sign-in for `remote` and waits for it: rclone serves its local link
/// (Dropbox's consent page behind it) and, when the user allows access, writes the token into its
/// own config. `on_link` gets that link as soon as rclone prints it, for SlopBar to open in a
/// browser; `cancel` stops rclone, which then writes nothing. Error codes: `cancelled`,
/// `timed_out`, and what `failure` reads from rclone's notices.
pub fn connect_dropbox(rclone: &std::path::Path, remote: &str, how: Connect, on_link: &dyn Fn(String), cancel: &std::sync::atomic::AtomicBool, timeout: std::time::Duration) -> Result<(), &'static str> {
    use std::io::BufRead;
    use std::process::Stdio;
    if !crate::rclone::valid_remote(remote) {
        return Err("rclone_failed");
    }
    let mut child = crate::sources::command(&rclone.to_string_lossy())
        .args(connect_args(remote, how))
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped())
        .spawn().map_err(|e| if refused_to_start(&e) { "rclone_blocked" } else { "rclone_failed" })?;
    // Ends with the app (an updater restart, a crash): a sign-in never outlives it.
    #[cfg(windows)]
    let _tree = crate::login::ProcessTree::attach(&child).ok();
    // rclone's notices go to its standard error. Its own loopback link is taken from them, and
    // its failure read; nothing else is kept or shown.
    let (send, lines) = std::sync::mpsc::channel();
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let started = std::time::Instant::now();
    let mut notices: Vec<String> = Vec::new();
    let take = |notices: &mut Vec<String>| {
        for line in lines.try_iter() {
            if let Some(link) = auth_link(&line) {
                on_link(link);
            } else if notices.len() < 64 {
                notices.push(line);
            }
        }
    };
    loop {
        take(&mut notices);
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                // Its last notices arrive just after it ends.
                std::thread::sleep(std::time::Duration::from_millis(200));
                take(&mut notices);
                return Err(failure(&notices));
            }
            Ok(None) => {}
            Err(_) => return Err("rclone_failed"),
        }
        let stop = if cancel.load(std::sync::atomic::Ordering::SeqCst) { Some("cancelled") }
            else if started.elapsed() > timeout { Some("timed_out") } else { None };
        if let Some(code) = stop {
            let _ = child.kill();
            let _ = child.wait();
            return Err(code);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Why rclone's sign-in failed, from its own (English, never translated) notices: `denied` (the
/// user did not allow access on Dropbox's page), `stale_page` (Allow was clicked on a page from an
/// earlier try), `port_busy` or `port_reserved` (its port 53682 is held by another program, or
/// reserved by Windows), else `rclone_failed`.
pub fn failure(notices: &[String]) -> &'static str {
    let said = |text: &str| notices.iter().any(|line| line.contains(text));
    if said("No code returned by remote server") {
        "denied"
    } else if said("Auth state doesn't match") {
        "stale_page"
    } else if said("failed to start auth webserver") {
        // Windows' own words come after rclone's, in English where Windows has it (Go asks for it).
        if said("forbidden by its access permissions") { "port_reserved" } else { "port_busy" }
    } else {
        "rclone_failed"
    }
}

/// Whether `state` is what rclone puts in its link: letters, digits, `-` and `_`, nothing else.
pub fn valid_state(state: &str) -> bool {
    !state.is_empty() && state.len() <= 128 && state.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The local link for a sign-in waiting on another computer, built here from its state alone:
/// this computer never opens text another computer sent.
pub fn link_for(state: &str) -> Option<String> {
    valid_state(state).then(|| format!("http://127.0.0.1:53682/auth?state={state}"))
}

/// The state part of rclone's link.
pub fn state_of(link: &str) -> Option<&str> {
    link.strip_prefix("http://127.0.0.1:53682/auth?state=").filter(|s| valid_state(s))
}

/// rclone's local sign-in link, as it prints it while it waits for the browser ("Please go to the
/// following link: http://127.0.0.1:53682/auth?state=…"). Only that exact form is taken: a
/// loopback address, rclone's port, and a plain state.
pub fn auth_link(line: &str) -> Option<String> {
    const START: &str = "http://127.0.0.1:53682/auth?state=";
    let link = line[line.find(START)?..].trim_end();
    valid_state(&link[START.len()..]).then(|| link.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_zip_for_each_windows_processor() {
        let x64 = asset("x86_64").unwrap();
        assert!(x64.file.contains(RCLONE_VERSION) && x64.file.ends_with("-windows-amd64.zip"));
        assert_eq!(x64.sha256.len(), 64);
        assert!(asset("aarch64").unwrap().file.ends_with("-windows-arm64.zip"));
        assert_eq!(asset("x86"), None);
        let [own, github] = urls(&x64);
        assert!(own.starts_with("https://downloads.rclone.org/v1.76.0/") && github.starts_with("https://github.com/rclone/rclone/releases/download/v1.76.0/"));
    }

    /// Downloads the real zip (about 25 MB) with the client the app uses:
    /// `cargo test -- --ignored downloads_and_unpacks`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn downloads_and_unpacks_the_pinned_rclone() {
        let dir = crate::test_support::TempDir::new();
        let dest = dir.0.join("rclone").join("rclone.exe");
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |p: u8| seen.lock().unwrap().push(p);
        let mut reasons = Vec::new();
        install_rclone(&dest, &std::sync::atomic::AtomicBool::new(false), &progress, &mut reasons).await.unwrap();
        assert!(reasons.is_empty(), "{reasons:?}");
        let out = std::process::Command::new(&dest).arg("version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("rclone v1.76.0"));
        assert!(!dest.parent().unwrap().join(".unpack").exists(), "the unpacking folder is removed");
        let seen = seen.into_inner().unwrap();
        assert!(seen.windows(2).all(|w| w[0] < w[1]) && seen.last() == Some(&100), "each percent once, up to 100: {seen:?}");

        // Cancelled, it stops before anything is put in place.
        let other = dir.0.join("cancelled").join("rclone.exe");
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert_eq!(install_rclone(&other, &cancel, &|_| {}, &mut reasons).await, Err("cancelled"));
        assert!(!other.exists());
    }

    /// The second source alone, rclone's GitHub release, installed as the app installs it (its own
    /// client): GitHub answers with a redirect to its file host, which the client follows (the
    /// zip, about 25 MB): `cargo test -- --ignored the_github_release`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn the_github_release_downloads_through_the_apps_client() {
        let asset = asset(std::env::consts::ARCH).unwrap();
        let github = urls(&asset)[1].clone();
        assert!(github.starts_with("https://github.com/"));
        // GitHub answers with a redirect, which the client follows to GitHub's file host.
        let answer = tokio::time::timeout(std::time::Duration::from_secs(60), download_client().get(&github).send()).await
            .expect("GitHub answers within a minute").unwrap();
        assert!(answer.status().is_success(), "{}", answer.status());
        assert!(answer.url().host_str().is_some_and(|h| h.ends_with(".githubusercontent.com")), "{}", crate::net::shown_host(answer.url()));
        drop(answer);
        let dir = crate::test_support::TempDir::new();
        let dest = dir.0.join("rclone").join("rclone.exe");
        let mut reasons = Vec::new();
        let installed = install_from(&[github], asset, &dest, &std::sync::atomic::AtomicBool::new(false), &|_| {}, &mut reasons).await;
        assert_eq!(installed, Ok(()), "{reasons:?}");
        assert!(reasons.is_empty(), "{reasons:?}");
        let out = std::process::Command::new(&dest).arg("version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("rclone v1.76.0"));
    }

    /// Where tar.exe is missing, Windows PowerShell unpacks the zip, its paths (with spaces) in
    /// the environment.
    #[cfg(windows)]
    #[test]
    fn powershell_unpacks_where_tar_is_missing() {
        let dir = crate::test_support::TempDir::new();
        let from = dir.0.join("with space");
        std::fs::create_dir_all(from.join("rclone-v1.76.0-windows-amd64")).unwrap();
        std::fs::write(from.join(r"rclone-v1.76.0-windows-amd64\rclone.exe"), b"synthetic").unwrap();
        let archive = dir.0.join("an archive.zip");
        let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
        let mut zip = crate::sources::command(&system.join(r"System32\tar.exe").to_string_lossy());
        zip.arg("-a").arg("-c").arg("-f").arg(&archive).arg("-C").arg(&from).arg("rclone-v1.76.0-windows-amd64");
        assert!(run_within(zip, std::time::Duration::from_secs(60)).is_ok_and(|out| out.status.success()), "made the test zip");
        let work = dir.0.join("unpack here");
        std::fs::create_dir_all(&work).unwrap();
        // Whichever shell started the tests: PowerShell 7 puts its own modules first in PSModulePath,
        // and Windows PowerShell must see only its own.
        let own = system.join(r"System32\WindowsPowerShell\v1.0\Modules");
        assert!(powershell_unpack(&system, &archive, &work).get_envs().any(|(k, v)| k == "PSModulePath" && v == Some(own.as_os_str())));
        let unpacked = run_within(powershell_unpack(&system, &archive, &work), UNPACK_LIMIT);
        assert!(unpacked.is_ok_and(|out| out.status.success()));
        assert_eq!(std::fs::read(work.join(r"rclone-v1.76.0-windows-amd64\rclone.exe")).unwrap(), b"synthetic");
        // A zip that is not one ends with a failure, never an exit 0 with nothing unpacked.
        let broken = dir.0.join("a broken archive.zip");
        std::fs::write(&broken, b"not a zip").unwrap();
        let other = dir.0.join("unpack broken");
        std::fs::create_dir_all(&other).unwrap();
        let failed = run_within(powershell_unpack(&system, &broken, &other), UNPACK_LIMIT);
        assert!(failed.is_ok_and(|out| !out.status.success()), "a broken zip fails");
    }

    #[test]
    fn an_rclone_that_windows_will_not_run_ends_the_setup_and_is_never_replaced() {
        use crate::rclone::{check, Ran, Runner, Unusable};
        /// `rclone version` as it went: it ran (exit code, output), or Windows gave this error.
        enum Version {
            Ran(Option<i32>, &'static str),
            Os(i32),
        }
        impl Runner for Version {
            fn run(&self, _: &[&str], _: Option<&[u8]>) -> std::io::Result<Ran> {
                match self {
                    Version::Ran(code, out) => Ok(Ran { code: *code, stdout: out.as_bytes().to_vec() }),
                    Version::Os(code) => Err(std::io::Error::from_raw_os_error(*code)),
                }
            }
        }
        // AppLocker, Application Control, antivirus, a restriction policy, a compatibility block,
        // access denied.
        for code in [1260, 4551, 225, 226, 786, 1259, if cfg!(windows) { 5 } else { 1260 }] {
            assert_eq!(check(&Version::Os(code)), Err(Unusable::Refused), "{code}");
        }
        assert_eq!(check(&Version::Os(2)), Err(Unusable::Failed), "a missing file is no refusal");
        assert_eq!(check(&Version::Ran(Some(0), "rclone v1.76.0\n- os/version: Windows")), Ok(()));
        assert_eq!(check(&Version::Ran(Some(0), "rclone v1.53.3")), Err(Unusable::Old));
        assert_eq!(check(&Version::Ran(Some(1), "")), Err(Unusable::Failed));
        assert_eq!(check(&Version::Ran(None, "")), Err(Unusable::Failed), "killed");
        assert_eq!(check(&Version::Ran(Some(0), "something else")), Err(Unusable::Failed));

        let own = std::path::PathBuf::from(r"C:\Users\someone\AppData\Local\SlopBar\rclone\rclone.exe");
        let installed = std::path::PathBuf::from(r"C:\Program Files\rclone\rclone.exe");
        assert_eq!(rclone_step(None, Some(&own)), RcloneStep::Download);
        assert_eq!(rclone_step(Some((installed.clone(), Ok(()))), Some(&own)), RcloneStep::Use(installed.clone()));
        assert_eq!(rclone_step(Some((installed.clone(), Err(Unusable::Old))), Some(&own)), RcloneStep::Download);
        // Refused to run, here or installed: stop, and download nothing to run from elsewhere.
        assert_eq!(rclone_step(Some((installed.clone(), Err(Unusable::Refused))), Some(&own)), RcloneStep::Stop("rclone_blocked"));
        assert_eq!(rclone_step(Some((own.clone(), Err(Unusable::Refused))), Some(&own)), RcloneStep::Stop("rclone_blocked"));
        // An installed one that fails is not replaced either; SlopBar's own broken copy is.
        assert_eq!(rclone_step(Some((installed, Err(Unusable::Failed))), Some(&own)), RcloneStep::Stop("rclone_broken"));
        assert_eq!(rclone_step(Some((own.clone(), Err(Unusable::Failed))), Some(&own)), RcloneStep::Download);
    }

    #[test]
    fn redirects_are_followed_only_over_https_to_rclones_and_githubs_hosts() {
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        for ok in [
            "https://downloads.rclone.org/v1.76.0/rclone-v1.76.0-windows-amd64.zip",
            "https://github.com/rclone/rclone/releases/download/v1.76.0/x.zip",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sig=SYNTHETIC",
            "https://objects.githubusercontent.com/x",
            "https://GITHUB.com/x",
        ] {
            assert!(download_url(&url(ok)), "{ok}");
        }
        for bad in [
            "http://github.com/x",
            "https://github.com:8443/x",
            "https://user:pass@github.com/x",
            "https://evilgithubusercontent.com/x",
            "https://githubusercontent.com.evil.example/x",
            "https://gateway.example.invalid/blocked",
            "https://140.82.112.3/x",
            "https://[::1]/x",
            "ftp://github.com/x",
        ] {
            assert!(!download_url(&url(bad)), "{bad}");
        }
    }

    // The tests below use the app's download client (`download_client_with`) against an HTTPS
    // server on this machine, made by a throwaway authority (`test_tls`). The client trusts that
    // authority and may follow redirects to `localhost` only; everything else is the app's.
    fn localhost(url: &reqwest::Url) -> bool {
        url.host_str() == Some("localhost")
    }

    struct Site {
        authority: crate::test_tls::Authority,
        server: crate::test_tls::Server,
    }

    impl Site {
        fn new(answer: impl Fn(&str, u16) -> crate::test_tls::Answer + Send + Sync + 'static) -> Site {
            let authority = crate::test_tls::Authority::new();
            let (cert, key) = authority.issue();
            let server = crate::test_tls::Server::start(cert, key, answer);
            Site { authority, server }
        }

        /// The app's download client, trusting this site's authority, allowing `localhost`, and
        /// reaching it directly whatever proxy this machine is set to use.
        fn client(&self) -> reqwest::Client {
            download_client_with(crate::net::builder().no_proxy().tls_certs_merge([self.authority.reqwest_root()]), localhost)
        }

        fn asked(&self) -> Vec<String> {
            self.server.asked.lock().unwrap().clone()
        }
    }

    const ZIP: &[u8] = b"PK\x03\x04 synthetic rclone zip";

    fn zip() -> crate::test_tls::Answer {
        crate::test_tls::Answer::Body { status: 200, content_type: "application/octet-stream", body: ZIP.to_vec() }
    }

    fn redirect(location: String) -> crate::test_tls::Answer {
        crate::test_tls::Answer::Redirect { status: 302, location }
    }

    async fn get(client: &reqwest::Client, urls: &[String]) -> (Result<Vec<u8>, &'static str>, Vec<Reason>) {
        let mut reasons = Vec::new();
        let got = download(client, urls, &sha256_hex(ZIP), &std::sync::atomic::AtomicBool::new(false), &|_| {}, &mut reasons).await;
        (got, reasons)
    }

    #[tokio::test]
    async fn a_redirect_to_an_allowed_host_is_followed() {
        let site = Site::new(|path, port| match path {
            "/release.zip" => redirect(format!("https://localhost:{port}/asset/1?sig=SYNTHETIC")),
            _ => zip(),
        });
        let (got, reasons) = get(&site.client(), &[site.server.url("/release.zip")]).await;
        assert_eq!(got, Ok(ZIP.to_vec()), "{reasons:?}");
        assert!(reasons.is_empty());
        assert_eq!(site.asked(), ["/release.zip", "/asset/1?sig=SYNTHETIC"]);
    }

    /// What app::http_client did: no redirect followed, so GitHub's link could never work.
    #[tokio::test]
    async fn a_client_that_follows_no_redirect_cannot_get_githubs_file() {
        let site = Site::new(|path, port| match path {
            "/release.zip" => redirect(format!("https://localhost:{port}/asset/1")),
            _ => zip(),
        });
        let none = crate::net::builder().no_proxy().tls_certs_merge([site.authority.reqwest_root()]).redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let (got, reasons) = get(&none, &[site.server.url("/release.zip")]).await;
        assert_eq!(got, Err("download_failed"));
        assert_eq!(reasons, [Reason { host: "localhost".into(), why: crate::net::Why::Redirected, status: Some(302), to: Some("localhost".into()) }]);
    }

    #[tokio::test]
    async fn a_redirect_elsewhere_is_not_followed_ends_the_download_and_names_only_the_host() {
        let site = Site::new(|path, port| match path {
            "/blocked.zip" => redirect("https://gateway.example.invalid/block?user=someone@example.invalid&sig=SECRET".into()),
            "/plain.zip" => redirect(format!("http://localhost:{port}/asset")),
            _ => zip(),
        });
        let (got, reasons) = get(&site.client(), &[site.server.url("/blocked.zip"), site.server.url("/next.zip")]).await;
        assert_eq!(got, Err("download_failed"));
        assert_eq!(reasons, [Reason { host: "localhost".into(), why: crate::net::Why::Redirected, status: Some(302), to: Some("gateway.example.invalid".into()) }]);
        assert_eq!(site.asked(), ["/blocked.zip"], "a redirect elsewhere may be a block: the next source is not tried");
        let shown = serde_json::to_string(&reasons).unwrap();
        assert!(!shown.contains('?') && !shown.contains('@') && !shown.contains("sig") && !shown.contains("SECRET") && !shown.contains("/block"), "{shown}");

        // Not over HTTPS: not followed either, and said so.
        let (_, reasons) = get(&site.client(), &[site.server.url("/plain.zip")]).await;
        assert_eq!(reasons[0].to.as_deref(), Some("http://localhost"));
    }

    #[tokio::test]
    async fn redirects_end_after_five() {
        let site = Site::new(|path, port| {
            let n: u32 = path.trim_start_matches("/hop").parse().unwrap_or(0);
            redirect(format!("https://localhost:{port}/hop{}", n + 1))
        });
        let (got, reasons) = get(&site.client(), &[site.server.url("/hop0")]).await;
        assert_eq!(got, Err("download_failed"));
        assert_eq!((reasons[0].why, reasons[0].status), (crate::net::Why::Redirected, Some(302)));
        assert_eq!(site.asked().len(), 1 + MAX_REDIRECTS, "{:?}", site.asked());
    }

    #[tokio::test]
    async fn a_page_where_the_zip_should_be_is_named_and_ends_the_download() {
        use crate::test_tls::Answer;
        let site = Site::new(|path, _| match path {
            "/html.zip" => Answer::Body { status: 200, content_type: "text/html; charset=utf-8", body: b"<html><title>Blocked by policy</title></html>".to_vec() },
            "/forbidden.zip" => Answer::Body { status: 403, content_type: "text/html", body: b"<html>Access denied</html>".to_vec() },
            "/text.zip" => Answer::Body { status: 200, content_type: "application/octet-stream", body: b"This download is blocked.".to_vec() },
            _ => zip(),
        });
        for (path, status) in [("/html.zip", 200), ("/forbidden.zip", 403), ("/text.zip", 200)] {
            let (got, reasons) = get(&site.client(), &[site.server.url(path), site.server.url("/next.zip")]).await;
            assert_eq!(got, Err("download_failed"), "{path}");
            assert_eq!(reasons, [Reason { host: "localhost".into(), why: crate::net::Why::Page, status: Some(status), to: None }], "{path}");
        }
        assert!(!site.asked().contains(&"/next.zip".to_string()), "a page may be a block: {:?}", site.asked());
    }

    #[tokio::test]
    async fn a_source_that_is_down_or_broken_lets_the_next_one_serve_the_zip() {
        use crate::test_tls::Answer;
        let site = Site::new(|path, _| match path {
            // As downloads.rclone.org's CDN answers: an HTML page with the status.
            "/gone.zip" => Answer::Body { status: 404, content_type: "text/html; charset=UTF-8", body: b"<html>404 Not Found</html>".to_vec() },
            "/down.zip" => Answer::Body { status: 503, content_type: "text/html", body: b"<html>Service Unavailable</html>".to_vec() },
            "/corrupt.zip" => Answer::Body { status: 200, content_type: "application/zip", body: b"PK\x03\x04 another zip".to_vec() },
            _ => zip(),
        });
        let urls = ["/gone.zip", "/down.zip", "/corrupt.zip", "/good.zip"].map(|p| site.server.url(p));
        let (got, reasons) = get(&site.client(), &urls).await;
        assert_eq!(got, Ok(ZIP.to_vec()));
        use crate::net::Why;
        assert_eq!(reasons.iter().map(|r| (r.why, r.status)).collect::<Vec<_>>(), [(Why::Status, Some(404)), (Why::Status, Some(503)), (Why::Mismatch, Some(200))]);

        // None left: the checksum's code when a zip came but did not match.
        let (got, _) = get(&site.client(), &urls[..3]).await;
        assert_eq!(got, Err("hash_mismatch"));
    }

    #[tokio::test]
    async fn a_certificate_for_another_name_is_named() {
        let site = Site::new(|_, _| zip());
        // The certificate is for `localhost`; the address is asked for instead.
        let (got, reasons) = get(&site.client(), &[format!("https://127.0.0.1:{}/a.zip", site.server.port)]).await;
        assert_eq!(got, Err("download_failed"));
        assert_eq!(reasons, [Reason { host: "127.0.0.1".into(), why: crate::net::Why::WrongName, status: None, to: None }]);
    }

    #[tokio::test]
    async fn a_certificate_this_computer_does_not_trust_is_named_and_ends_the_download() {
        let site = Site::new(|_, _| zip());
        // The app's own client (reaching this machine directly): the throwaway authority is in no
        // store here.
        let client = download_client_with(crate::net::builder().no_proxy(), download_url);
        let (got, reasons) = get(&client, &[site.server.url("/a.zip"), site.server.url("/b.zip")]).await;
        assert_eq!(got, Err("download_failed"));
        assert_eq!(reasons, [Reason { host: "localhost".into(), why: crate::net::Why::Untrusted, status: None, to: None }]);
        assert!(site.asked().is_empty(), "the request never got past the certificate");
    }

    #[cfg(windows)]
    #[test]
    fn a_helper_that_hangs_is_ended_at_its_limit() {
        let started = std::time::Instant::now();
        let mut hang = crate::sources::command("ping");
        hang.args(["-n", "30", "127.0.0.1"]);
        assert!(matches!(run_within(hang, std::time::Duration::from_secs(1)), Err(None)), "killed, not waited for");
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        let mut echo = crate::sources::command("cmd");
        echo.args(["/c", "echo", "rclone", "v1.76.0"]);
        let out = run_within(echo, std::time::Duration::from_secs(20)).unwrap();
        assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).starts_with("rclone v1.76.0"));
    }

    #[test]
    fn every_download_and_unpacking_step_has_a_limit() {
        assert!(DOWNLOAD_LIMIT <= std::time::Duration::from_secs(15 * 60) && STALL < DOWNLOAD_LIMIT);
        assert!(UNPACK_LIMIT <= std::time::Duration::from_secs(5 * 60) && VERSION_LIMIT <= std::time::Duration::from_secs(60));
        // No helper is run without one, and the download is never unbounded.
        let code = include_str!("sharing_setup.rs");
        let body = &code[code.find("fn extract(").unwrap()..code.find("/// How rclone gets a Dropbox remote here").unwrap()];
        assert!(!body.contains(".status()") && !body.contains(".output()"), "every helper in extract runs through run_within");
        assert!(code.contains(".timeout(DOWNLOAD_LIMIT)"));
    }

    #[test]
    fn an_installed_rclone_is_used_from_v1_60_on() {
        for (version, ok) in [
            ("rclone v1.76.0\n- os/version: Microsoft Windows 10 Pro", true),
            ("rclone v1.74.4", true),
            ("rclone v1.60.0", true),
            ("rclone v1.59.2", false),
            ("rclone v1.53.3-DEV", false),
            ("rclone v2.0.0", true),
            ("rclone version unknown", false),
            ("", false),
            ("not rclone v1.76.0", false),
        ] {
            assert_eq!(recent_enough(version), ok, "{version}");
        }
    }

    #[test]
    fn rclone_signs_in_without_its_own_browser_or_output() {
        assert_eq!(connect_args("dropbox", Connect::Create),
            ["config", "create", "dropbox", "dropbox", "config_auth_no_browser=true", "--no-output", "--ask-password=false"]);
        // An existing remote keeps its settings and gets a new sign-in; `config create` would
        // delete it first.
        assert_eq!(connect_args("dropbox", Connect::Reconnect),
            ["config", "update", "dropbox", "config_refresh_token=true", "config_auth_no_browser=true", "--no-output", "--ask-password=false"]);
    }

    /// rclone v1.76.0's own notices for each way its sign-in fails (state values replaced).
    #[test]
    fn each_sign_in_failure_has_its_own_reason() {
        let lines = |text: &str| text.lines().map(String::from).collect::<Vec<_>>();
        let waiting = "2026/11/15 12:06:47 NOTICE: Please go to the following link: http://127.0.0.1:53682/auth?state=S\n2026/11/15 12:06:47 NOTICE: Waiting for code...\n";
        assert_eq!(failure(&lines(&format!("{waiting}Error: config failed to refresh token: Error: Auth Error\nCode: \"\"\nDescription: No code returned by remote server: access_denied: The user chose not to give your app access\nHelp:"))), "denied");
        assert_eq!(failure(&lines(&format!("{waiting}Error: config failed to refresh token: Error: Auth state doesn't match\nCode: \"\"\nDescription: Expecting \"S\" got \"T\""))), "stale_page");
        assert_eq!(failure(&lines("Error: config failed to refresh token: failed to start auth webserver: listen tcp 127.0.0.1:53682: bind: Only one usage of each socket address (protocol/network address/port) is normally permitted.")), "port_busy");
        assert_eq!(failure(&lines("Error: config failed to refresh token: failed to start auth webserver: listen tcp 127.0.0.1:53682: bind: An attempt was made to access a socket in a way forbidden by its access permissions.")), "port_reserved");
        // Windows' words in another language still say the port.
        assert_eq!(failure(&lines("Error: config failed to refresh token: failed to start auth webserver: listen tcp 127.0.0.1:53682: bind: Normalerweise darf jede Socketadresse nur jeweils einmal verwendet werden.")), "port_busy");
        assert_eq!(failure(&lines(&format!("{waiting}Error: failed to get token: Post \"https://api.dropbox.com/oauth2/token\": dial tcp: lookup api.dropbox.com: no such host"))), "rclone_failed");
        assert_eq!(failure(&[]), "rclone_failed");
    }

    #[test]
    fn a_zip_that_does_not_match_its_checksum_is_refused() {
        assert_ne!(sha256_hex(b"not rclone"), asset("x86_64").unwrap().sha256);
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn the_link_is_built_here_from_a_plain_state_and_never_serialized() {
        assert_eq!(link_for("Ab-9_x").as_deref(), Some("http://127.0.0.1:53682/auth?state=Ab-9_x"));
        for bad in ["", "a b", "a&next=https://evil.example", r"\\server\share", "x".repeat(129).as_str()] {
            assert_eq!(link_for(bad), None, "{bad}");
        }
        assert_eq!(state_of("http://127.0.0.1:53682/auth?state=Ab-9_x"), Some("Ab-9_x"));
        assert_eq!(state_of("http://evil.example/auth?state=Ab"), None);
        let setup = Setup { phase: Phase::Dropbox, link: Some("http://127.0.0.1:53682/auth?state=Ab-9_x".into()), waiting: true, ..Default::default() };
        let shown = serde_json::to_string(&setup).unwrap();
        assert!(!shown.contains("Ab-9_x") && shown.contains("\"phase\":\"dropbox\""), "{shown}");
        assert!(Phase::Dropbox.running() && !Phase::Done.running() && !Phase::Failed.running());
    }

    #[test]
    fn only_rclones_own_loopback_link_is_taken() {
        let line = "2026/11/15 12:06:34 NOTICE: Please go to the following link: http://127.0.0.1:53682/auth?state=Ab-9_x";
        assert_eq!(auth_link(line).as_deref(), Some("http://127.0.0.1:53682/auth?state=Ab-9_x"));
        assert_eq!(auth_link(&format!("{line}\r")).as_deref(), Some("http://127.0.0.1:53682/auth?state=Ab-9_x"));
        assert_eq!(auth_link("NOTICE: If your browser doesn't open automatically go to the following link: http://127.0.0.1:53682/auth?state=Ab-9_x").as_deref(),
            Some("http://127.0.0.1:53682/auth?state=Ab-9_x"));
        for bad in [
            "NOTICE: Waiting for code...",
            "NOTICE: Make sure your Redirect URL is set to \"http://localhost:53682/\" in your custom config.",
            "go to http://127.0.0.1:53682/auth?state=",
            "go to http://127.0.0.1:53682/auth?state=a&redirect=https://evil.example",
            "go to http://127.0.0.1:53682/auth?state=a b",
            "go to http://10.0.0.5:53682/auth?state=abc",
        ] {
            assert_eq!(auth_link(bad), None, "{bad}");
        }
    }
}
