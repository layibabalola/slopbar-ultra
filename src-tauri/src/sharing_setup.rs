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

/// The zip is about 25 MB; anything far larger is not it.
const MAX_ZIP: u64 = 128 * 1024 * 1024;

/// A download that receives nothing for this long has stalled. It has no overall limit: a slow
/// link may take minutes, and Cancel ends it sooner.
const STALL: std::time::Duration = std::time::Duration::from_secs(30);

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

/// Downloads the pinned rclone zip (its own site, then its GitHub release), takes it only when
/// its SHA-256 matches, and puts its `rclone.exe` at `dest`. `progress` hears each new percent;
/// `cancel` ends the download. Error codes: `rclone_unsupported`, `download_failed`,
/// `hash_mismatch`, `extract_failed`, `cancelled`.
pub async fn install_rclone(client: &reqwest::Client, dest: &std::path::Path, cancel: &std::sync::atomic::AtomicBool, progress: &(dyn Fn(u8) + Send + Sync)) -> Result<(), &'static str> {
    let asset = asset(std::env::consts::ARCH).ok_or("rclone_unsupported")?;
    let mut failure = "download_failed";
    for url in urls(&asset) {
        let bytes = match fetch(client, &url, cancel, progress).await {
            Ok(bytes) => bytes,
            Err("cancelled") => return Err("cancelled"),
            Err(_) => continue,
        };
        if sha256_hex(&bytes) != asset.sha256 {
            failure = "hash_mismatch";
            continue;
        }
        let dest = dest.to_path_buf();
        return tokio::task::spawn_blocking(move || extract(&bytes, &asset, &dest)).await.unwrap_or(Err("extract_failed"));
    }
    Err(failure)
}

async fn fetch(client: &reqwest::Client, url: &str, cancel: &std::sync::atomic::AtomicBool, progress: &(dyn Fn(u8) + Send + Sync)) -> Result<Vec<u8>, &'static str> {
    fn failed<E>(_: E) -> &'static str {
        "download_failed"
    }
    // The client's own limit is for small requests; this one is bounded by STALL instead.
    let send = client.get(url).timeout(std::time::Duration::from_secs(60 * 60)).send();
    let mut response = tokio::time::timeout(STALL, send).await.map_err(failed)?.map_err(failed)?;
    if !response.status().is_success() || response.content_length().is_some_and(|n| n > MAX_ZIP) {
        return Err("download_failed");
    }
    let total = response.content_length().filter(|n| *n > 0);
    let (mut bytes, mut shown) = (Vec::new(), None);
    loop {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("cancelled");
        }
        let Some(chunk) = tokio::time::timeout(STALL, response.chunk()).await.map_err(failed)?.map_err(failed)? else { break };
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > MAX_ZIP {
            return Err("download_failed");
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
        let unpacked = if tar.is_file() {
            crate::sources::command(&tar.to_string_lossy()).arg("-xf").arg(&archive).arg("-C").arg(&work).arg(&inner).status().is_ok_and(|s| s.success())
        } else {
            let ps = system.join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
            crate::sources::command(&ps.to_string_lossy())
                .args(["-NoProfile", "-NonInteractive", "-Command", "Expand-Archive -LiteralPath $args[0] -DestinationPath $args[1] -Force"])
                .arg(&archive).arg(&work).status().is_ok_and(|s| s.success())
        };
        let exe = work.join(&inner);
        if !unpacked || !exe.is_file() {
            return Err("extract_failed");
        }
        let version = crate::sources::command(&exe.to_string_lossy()).arg("version").output().map_err(|_| "extract_failed")?;
        if !String::from_utf8_lossy(&version.stdout).starts_with(&format!("rclone {RCLONE_VERSION}")) {
            return Err("extract_failed");
        }
        let _ = std::fs::remove_file(dest);
        std::fs::rename(&exe, dest).map_err(|_| "extract_failed")
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
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
        .spawn().map_err(|_| "rclone_failed")?;
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

    /// Downloads the real zip (about 25 MB): `cargo test -- --ignored downloads_and_unpacks`.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn downloads_and_unpacks_the_pinned_rclone() {
        let dir = crate::test_support::TempDir::new();
        let dest = dir.0.join("rclone").join("rclone.exe");
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |p: u8| seen.lock().unwrap().push(p);
        install_rclone(&reqwest::Client::new(), &dest, &std::sync::atomic::AtomicBool::new(false), &progress).await.unwrap();
        let out = std::process::Command::new(&dest).arg("version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("rclone v1.76.0"));
        assert!(!dest.parent().unwrap().join(".unpack").exists(), "the unpacking folder is removed");
        let seen = seen.into_inner().unwrap();
        assert!(seen.windows(2).all(|w| w[0] < w[1]) && seen.last() == Some(&100), "each percent once, up to 100: {seen:?}");

        // Cancelled, it stops before anything is put in place.
        let other = dir.0.join("cancelled").join("rclone.exe");
        let cancel = std::sync::atomic::AtomicBool::new(true);
        assert_eq!(install_rclone(&reqwest::Client::new(), &other, &cancel, &|_| {}).await, Err("cancelled"));
        assert!(!other.exists());
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
