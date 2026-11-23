//! Pairing: an agent on one of the user's computers brings another onto the account roster over
//! SSH (OpenSSH or Tailscale SSH), so nobody types a roster passphrase on each machine.
//!
//! The source computer's roster passphrase (or, when it has none, a new random one) is the
//! pairing key. It goes from this process straight into `ssh`'s standard input and from there into
//! the target's `slopbar-mcp --pair-receive`, which hands it to the target app's door; each app
//! keeps it as it keeps every roster passphrase (DPAPI, see `roster::store_passphrase`). It is
//! never on a command line, in a result, in an error or in a log, so the agent driving this never
//! sees it either.
//!
//! The same SSH channel answers readiness questions (`readiness`: build, desktop session, browser,
//! roster, accounts waiting) and runs the target's own tools (`--call`), whose sign-ins the
//! target's running app opens on its desktop, never in the SSH session.

use std::io::{Read, Write};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::backup::Passphrase;
use crate::door::Request;

pub const OFFER_FORMAT: &str = "slopbar-pairing";
const OFFER_VERSION: u32 = 1;
/// An offer is one JSON line; a roster passphrase is at most 1024 characters of up to 4 bytes.
const MAX_OFFER: u64 = 16 * 1024;
/// A remote tool's answer (a usage snapshot is the largest).
const MAX_ANSWER: u64 = 4 * 1024 * 1024;
/// Random bytes in a generated key: 256 bits, far past any guessing.
const KEY_BYTES: usize = 32;
const KEY_PREFIX: &str = "slopbar-pair-";
/// Connecting, and the remote tool, together. Sign-ins return at once, so this is generous.
const SSH_TIMEOUT: Duration = Duration::from_secs(120);
/// Tools another computer may be asked to run through `run_on_machine`: what bringing it up
/// needs, and nothing that takes a secret.
pub const REMOTE_TOOLS: [&str; 15] = ["get_machine_readiness", "get_app_info", "get_health", "get_usage", "get_settings", "list_sharing_locations", "get_sharing_setup",
    "get_roster", "sync_roster", "update_settings", "sign_in_all", "stop_sign_ins", "connect_monitoring_all", "refresh_usage", "restart_app"];

/// What the source sends the target, on `ssh`'s standard input only.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub format: String,
    pub v: u32,
    pub key: Passphrase,
    /// The rclone remote the source shares through; None when it uses a synced folder, whose
    /// path differs per computer.
    pub presence_remote: Option<String>,
    pub from: String,
    /// Replace a different roster passphrase the target already has (which its other peers
    /// share): only when the user said so.
    #[serde(default)]
    pub replace: bool,
}

/// Settings `run_on_machine` may change on another computer: where it shares and whether, and
/// its sign-in window. Never phone alerts, aliases or anything else.
pub const REMOTE_SETTINGS: [&str; 4] = ["presence_remote", "presence_folder", "account_roster", "signin_window"];

/// A new pairing key: 256 random bits as lowercase base32.
pub fn generate_key() -> Passphrase {
    use chacha20poly1305::aead::{rand_core::RngCore, OsRng};
    let mut bytes = [0u8; KEY_BYTES];
    OsRng.fill_bytes(&mut bytes);
    Passphrase(format!("{KEY_PREFIX}{}", base32(&bytes)))
}

fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let (mut out, mut buffer, mut bits) = (String::new(), 0u32, 0u32);
    for &b in bytes {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// An SSH destination as `ssh` takes it: a ~/.ssh/config alias, host name, Tailscale name or
/// address, optionally `user@`. Never something `ssh` could read as an option.
pub fn valid_host(host: &str) -> bool {
    let ok_chars = host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':'));
    (1..=253).contains(&host.len()) && ok_chars && !host.starts_with('-') && host.matches('@').count() <= 1 && !host.ends_with('@')
}

/// The command `ssh` runs on the target: its `slopbar-mcp.exe` with `args`, under PowerShell 7
/// (`pwsh`). It reads the same from cmd, Windows PowerShell or PowerShell 7 as the remote shell (an
/// encoded command needs no quoting). `args` never holds a secret: a command line is visible to
/// every process on the target. `run_there` sends it, and the same script under Windows
/// PowerShell 5.1 (`fallback_command`) only where pwsh never started it: a 5.1 process started
/// over SSH can outlive its session and grow without bound (it reached 85 GiB on one computer).
pub fn remote_command(args: &[&str]) -> String {
    command_under("pwsh", args)
}

/// `remote_command`'s script under Windows PowerShell 5.1, for a target without PowerShell 7.
fn fallback_command(args: &[&str]) -> String {
    command_under("powershell", args)
}

/// The remote script's first line, printed before anything that can fail: the PowerShell it was
/// sent to started it. `answer` skips it, as it skips anything but the last JSON line.
const STARTED: &str = "slopbar-remote: started";

fn command_under(shell: &str, args: &[&str]) -> String {
    let quoted: Vec<String> = args.iter().map(|a| format!("'{}'", a.replace('\'', "''"))).collect();
    let script = format!(concat!(
        "'{}'\n",
        "$ErrorActionPreference = 'Stop'\n",
        "$exe = Join-Path $env:LOCALAPPDATA 'SlopBar\\slopbar-mcp.exe'\n",
        "if (-not (Test-Path -LiteralPath $exe)) {{ '{{\"error\":\"slopbar_not_installed\"}}'; exit 3 }}\n",
        // A pipeline stage, not just capturing it, makes either PowerShell wait for this windowless
        // program; it still inherits standard input.
        "$out = & $exe {} | ForEach-Object {{ $_ }}\n",
        "$code = $LASTEXITCODE\n",
        "$out\n",
        "exit $code\n"), STARTED, quoted.join(" "));
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD,
        script.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>());
    format!("{shell} -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {encoded}")
}

/// Runs the target's `slopbar-mcp.exe` with `args`, `input` as its standard input, and returns
/// what it printed: under PowerShell 7, or under Windows PowerShell 5.1 where pwsh never started
/// the script (not installed there, or it could not start). Once the script printed `STARTED`
/// under pwsh, its outcome stands, whatever came after, so no tool runs twice and 5.1 never starts
/// where pwsh runs. The exit code cannot tell these apart: a PowerShell remote shell turns every
/// failure into 1. The marker is looked for anywhere in the output, since a shell may print its
/// own text first, even without a line break (cmd's AutoRun, a profile's greeting, a title); only
/// the script can print it, as the command line carries it only encoded. `input` goes to both
/// tries; a shell that cannot start pwsh exits without reading it.
fn run_there(remote: &dyn Remote, host: &str, args: &[&str], input: &[u8]) -> Result<Vec<u8>, String> {
    run_there_noting(remote, host, args, input).map(|(out, _)| out)
}

/// `run_there`, also saying whether the script went to Windows PowerShell (pwsh never started it).
fn run_there_noting(remote: &dyn Remote, host: &str, args: &[&str], input: &[u8]) -> Result<(Vec<u8>, bool), String> {
    let out = remote.run(host, &remote_command(args), input)?;
    if String::from_utf8_lossy(&out).contains(STARTED) {
        return Ok((out, false));
    }
    Ok((remote.run(host, &fallback_command(args), input)?, true))
}

/// JSON with every non-ASCII character escaped, so no console code page on the way through SSH
/// can garble an email or machine name.
pub fn ascii_json(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{\"error\":\"unserializable\"}".into());
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The SSH host each paired computer answered on, by its machine id (the one its roster carries),
// so this computer's app can reach it too: Retry on Settings > Accounts' "Signing in on <machine>".

const HOSTS_FILE: &str = "paired-machines.json";
const MAX_HOSTS: usize = 64;

fn hosts(dir: &std::path::Path) -> serde_json::Map<String, Value> {
    std::fs::read(dir.join(HOSTS_FILE)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

/// Notes that the computer `machine_id` answered on `host` (from its readiness, which carries the id).
pub fn remember_host(dir: &std::path::Path, machine_id: &str, host: &str) {
    if !valid_host(host) || crate::roster::file_name(machine_id).as_deref() != Some(&format!("{machine_id}.roster")) {
        return;
    }
    let mut map = hosts(dir);
    if map.get(machine_id).and_then(Value::as_str) == Some(host) {
        return;
    }
    map.remove(machine_id);
    while map.len() >= MAX_HOSTS {
        let Some(first) = map.keys().next().cloned() else { break };
        map.remove(&first);
    }
    map.insert(machine_id.into(), host.into());
    let _ = crate::private_store::write(&dir.join(HOSTS_FILE), &serde_json::to_vec_pretty(&map).unwrap_or_default());
}

/// Every computer paired or checked from here: its machine id and the SSH host it answered on.
pub fn paired(dir: &std::path::Path) -> Vec<(String, String)> {
    hosts(dir).into_iter().filter_map(|(id, host)| Some((id, host.as_str().filter(|h| valid_host(h))?.to_owned()))).collect()
}

/// Notes that the computer `machine_id` answers on `host`, only when no host is noted for it yet:
/// a host found by name (see `relay`) never re-points one that was paired or checked.
pub fn remember_new_host(dir: &std::path::Path, machine_id: &str, host: &str) {
    if host_for(dir, machine_id).is_none() {
        remember_host(dir, machine_id, host);
    }
}

/// The SSH host `machine_id` last answered on here, if it was ever checked or paired from here.
pub fn host_for(dir: &std::path::Path, machine_id: &str) -> Option<String> {
    hosts(dir).get(machine_id).and_then(Value::as_str).filter(|h| valid_host(h)).map(str::to_owned)
}

fn remember_answer(host: &str, readiness: &Value) {
    if let (Some(dir), Some(id)) = (crate::settings::app_dir(), readiness.pointer("/machine/id").and_then(Value::as_str)) {
        remember_host(&dir, id, host);
    }
}

// ---------------------------------------------------------------------------------------------
// What pairing talks to, so tests can stand in for the app and the other computer.

pub trait Door {
    fn call(&self, request: &Request) -> Result<Value, String>;
}

/// The app running here, through its door.
pub struct LocalDoor(pub String);

impl Door for LocalDoor {
    fn call(&self, request: &Request) -> Result<Value, String> {
        crate::door::call(&self.0, request)
    }
}

pub trait Remote {
    /// Runs `command` on `host` with `input` as its standard input; returns its standard output.
    fn run(&self, host: &str, command: &str, input: &[u8]) -> Result<Vec<u8>, String>;

    /// `run`, given up after `limit` (and, over SSH, on a connection that stops answering).
    fn run_within(&self, host: &str, command: &str, input: &[u8], limit: Duration) -> Result<Vec<u8>, String> {
        let _ = limit;
        self.run(host, command, input)
    }
}

/// The system's OpenSSH client, with the user's own keys and ~/.ssh/config (Tailscale SSH works
/// through it too). It never asks for a password or accepts an unknown host key.
pub struct Ssh;

impl Ssh {
    pub fn exe() -> std::path::PathBuf {
        let system = std::env::var_os("SystemRoot").map(|r| std::path::PathBuf::from(r).join(r"System32\OpenSSH\ssh.exe"));
        system.filter(|p| p.is_file()).unwrap_or_else(|| "ssh".into())
    }
}

impl Remote for Ssh {
    fn run(&self, host: &str, command: &str, input: &[u8]) -> Result<Vec<u8>, String> {
        ssh_run(host, command, input, SSH_TIMEOUT, &[])
    }

    fn run_within(&self, host: &str, command: &str, input: &[u8], limit: Duration) -> Result<Vec<u8>, String> {
        // A connection that stops answering is given up in 15 seconds, not at the limit; and no
        // port forward ~/.ssh/config names for the host rides along with a relay call.
        ssh_run(host, command, input, limit, &["-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3", "-o", "ClearAllForwardings=yes"])
    }
}

/// The system's `ssh` running `command` on `host`, given up after `limit`; `options` go before the host.
fn ssh_run(host: &str, command: &str, input: &[u8], limit: Duration, options: &[&str]) -> Result<Vec<u8>, String> {
    use std::process::{Command, Stdio};
    if !valid_host(host) {
        return Err("bad_ssh_host".into());
    }
    let mut cmd = Command::new(Ssh::exe());
    cmd.args(["-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=15"]).args(options).args(["-T", host, command])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: this server has no console.
    }
    let mut child = cmd.spawn().map_err(|_| "ssh_missing".to_string())?;
    let mut stdin = child.stdin.take().ok_or("ssh_failed")?;
    let mut input = input.to_vec();
    // The input may be a pairing key: this copy is wiped once sent, a best effort like `pair`'s
    // and `receive`'s wipes (other copies of a key stay in memory until reused).
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
        input.iter_mut().for_each(|b| *b = 0);
    });
    let mut stdout = child.stdout.take().ok_or("ssh_failed")?;
    let mut stderr = child.stderr.take().ok_or("ssh_failed")?;
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stdout).take(MAX_ANSWER).read_to_end(&mut out);
        let _ = send.send(out);
    });
    let errors = std::thread::spawn(move || {
        let mut err = Vec::new();
        let _ = (&mut stderr).take(16 * 1024).read_to_end(&mut err);
        err
    });
    let Ok(out) = receive.recv_timeout(limit) else {
        let _ = child.kill();
        return Err("ssh_timeout".into());
    };
    let _ = writer.join();
    // Standard output closed (or hit its bound): the exit follows at once, or never.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return Err("ssh_timeout".into());
            }
        }
    };
    finished(out, status.code(), &errors.join().unwrap_or_default())
}

/// What a finished `ssh` gives: the command's output, or ssh's own failure.
fn finished(out: Vec<u8>, code: Option<i32>, err: &[u8]) -> Result<Vec<u8>, String> {
    // 255 is ssh's own failure (no connection, key refused, unknown host key); "**" lines are
    // its banners (a key exchange warning), not the reason.
    if code == Some(255) {
        let first = String::from_utf8_lossy(err).lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("**")).unwrap_or("").chars().take(200).collect::<String>();
        return Err(format!("ssh_failed: {first}"));
    }
    Ok(out)
}

/// Runs one of the target's tools through its `slopbar-mcp --call`; its answer or its error.
pub fn remote_call(remote: &dyn Remote, host: &str, tool: &str, arguments: &Value) -> Result<Value, RemoteError> {
    read(&run_there(remote, host, &["--call-b64", &asking(tool, arguments)], b"").map_err(RemoteError::here)?)
}

/// Why a call to another computer failed. `text` is what an agent reads (that computer words its
/// errors before printing them) and what this becomes as a `String`; `code` is what code compares,
/// never the words, which differ between versions and brands.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteError {
    pub code: Option<String>,
    pub text: String,
}

/// What a SlopBar from before `code` came (see `mcp::error_line`) printed for the codes this computer
/// acts on, as their only sign: matched whole. Their wording is fixed in those releases, so it stays
/// here whatever `explain` says now.
const EARLIER_WORDS: [(&str, &str); 1] = [
    ("sharing_setup_busy", "A sharing setup is already running on this computer: follow it with get_sharing_setup, or stop it with setup_sharing cancel: true."),
];

impl RemoteError {
    /// An error this computer met on the way (`ssh_failed`, `ssh_timeout`…): already a code.
    fn here(error: String) -> Self {
        RemoteError { code: crate::mcp::is_code(&error).then(|| error.clone()), text: error }
    }

    /// The error another computer printed: its `code` when it gave one, else the code its words
    /// stand for (`EARLIER_WORDS`), else the words themselves when they are a code (one `explain`
    /// leaves as it is).
    fn printed(text: &str, code: Option<&str>) -> Self {
        let code = code.filter(|c| crate::mcp::is_code(c)).map(str::to_owned)
            .or_else(|| EARLIER_WORDS.iter().find(|(_, words)| *words == text).map(|(code, _)| (*code).to_owned()))
            .or_else(|| crate::mcp::is_code(text).then(|| text.to_owned()));
        RemoteError { code, text: text.to_owned() }
    }

    /// Whether it failed with `code`.
    pub fn is(&self, code: &str) -> bool {
        self.code.as_deref() == Some(code)
    }
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl From<RemoteError> for String {
    fn from(error: RemoteError) -> String {
        error.text
    }
}

/// How long a relay call may take over SSH beyond the waiting it asks the target to do.
const RELAY_MARGIN: Duration = Duration::from_secs(25);

/// Sends one relay request (`RelayPlan`, `RelayStart`, `RelayState` or `RelayStop`) to the
/// target's `slopbar-mcp --relay` (see `relay`), allowing `wait` for the waiting it asks for; its
/// answer or its error code as the target gave it. PowerShell 7 only: a sitting makes dozens of
/// these calls, and Windows PowerShell 5.1 started over SSH can outlive its session
/// (`no_pwsh_over_ssh` instead). The request names accounts and ids, never a secret (a command line
/// is visible to every process there).
pub fn relay_call(remote: &dyn Remote, host: &str, request: &Request, wait: Duration) -> Result<Value, String> {
    let text = serde_json::to_string(request).map_err(|e| e.to_string())?;
    let encoded = base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, text);
    let out = remote.run_within(host, &remote_command(&["--relay", &encoded]), b"", wait + RELAY_MARGIN)?;
    if !String::from_utf8_lossy(&out).contains(STARTED) {
        return Err("no_pwsh_over_ssh".into());
    }
    answer(&out)
}

/// `--call-b64`'s argument: the tool and its arguments, as base64 JSON.
fn asking(tool: &str, arguments: &Value) -> String {
    let ask = json!({ "tool": tool, "arguments": arguments });
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, ask.to_string())
}

/// Readiness's code, and its advice, when Windows PowerShell ran SlopBar's command over SSH there.
const NO_PWSH_OVER_SSH: &str = "no_pwsh_over_ssh";
const NO_PWSH_ADVICE: &str = "Over SSH, pwsh did not start there, so SlopBar used Windows PowerShell: install PowerShell 7 (winget install --id Microsoft.PowerShell).";

/// Another computer's readiness over SSH, with what this side saw: which PowerShell ran SlopBar's
/// command there (`ssh_powershell`). Where pwsh never started it, Windows PowerShell did, which is a
/// problem of its own there (`no_pwsh_over_ssh`), noted after whether sign-ins can start was decided
/// there, so it never stops them.
pub fn remote_readiness(remote: &dyn Remote, host: &str, arguments: &Value) -> Result<Value, String> {
    let (out, windows_powershell) = run_there_noting(remote, host, &["--call-b64", &asking("get_machine_readiness", arguments)], b"")?;
    let mut answer = answer(&out)?;
    if let Some(fields) = answer.as_object_mut() {
        fields.insert("ssh_powershell".into(), (if windows_powershell { "windows_powershell" } else { "pwsh" }).into());
        if windows_powershell {
            if let Some(problems) = fields.get_mut("problems").and_then(Value::as_array_mut) {
                problems.push(NO_PWSH_OVER_SSH.into());
            }
            let next = format!("{} {NO_PWSH_ADVICE}", fields.get("next").and_then(Value::as_str).unwrap_or_default());
            fields.insert("next".into(), next.trim_start().into());
        }
    }
    Ok(answer)
}

/// The JSON object a remote `slopbar-mcp` printed last: `{"ok": …}` or `{"error": …}`, its error as
/// it was printed (see `read` for its code).
fn answer(out: &[u8]) -> Result<Value, String> {
    read(out).map_err(String::from)
}

/// `answer`, with the error's code (`RemoteError`).
fn read(out: &[u8]) -> Result<Value, RemoteError> {
    let no_answer = || RemoteError::here("remote_no_answer".into());
    let text = String::from_utf8_lossy(out);
    let last = text.lines().map(str::trim).rfind(|l| l.starts_with('{')).ok_or_else(no_answer)?;
    let mut value: Value = serde_json::from_str(last).map_err(|_| no_answer())?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(RemoteError::printed(error, value.get("code").and_then(Value::as_str)));
    }
    value.get_mut("ok").map(Value::take).ok_or_else(no_answer)
}

// ---------------------------------------------------------------------------------------------
// Pairing

/// The source's half: checks the target is up, makes sure this computer shares its roster with a
/// key (its own passphrase, or a new random one), sends the key to `host` and syncs both.
/// `saved` is this computer's saved roster passphrase, if any.
pub fn pair(door: &dyn Door, remote: &dyn Remote, host: &str, saved: Option<Passphrase>, machine: &str, replace: bool) -> Result<Value, String> {
    if !valid_host(host) {
        return Err("bad_ssh_host".into());
    }
    let settings = door.call(&Request::GetSettings)?;
    let presence_remote = settings.get("presence_remote").and_then(Value::as_str).map(str::to_owned);
    let folder = settings.get("presence_folder").and_then(Value::as_str).is_some();
    if presence_remote.is_none() && !folder {
        return Err("no_sharing_location".into());
    }
    // The target first: nothing changes here when it cannot take the key.
    let target = remote_call(remote, host, "get_machine_readiness", &json!({})).map_err(|e| format!("target: {e}"))?;
    if target.pointer("/app/running") != Some(&Value::Bool(true)) {
        return Err("target_not_running".into());
    }
    // And that it can share where this computer does, before anything changes here.
    match &presence_remote {
        Some(name) => {
            let places = remote_call(remote, host, "list_sharing_locations", &json!({})).map_err(|e| format!("target: {e}"))?;
            // rclone itself must list the remote there: the setting alone shares nothing.
            if !lists_remote(&places, name) {
                return Err(format!("target: remote_missing_here: {name}"));
            }
        }
        None if target.pointer("/roster/location") != Some(&Value::Bool(true)) => return Err("no_sharing_location_here".into()),
        None => {}
    }
    if settings.get("account_roster") != Some(&Value::Bool(true)) {
        door.call(&Request::UpdateSettings { patch: json!({ "account_roster": true }) })?;
    }
    let key = match saved {
        Some(key) => (key, false),
        None => {
            let key = generate_key();
            door.call(&Request::SetRosterPassphrase { passphrase: Some(key.clone()) })?;
            (key, true)
        }
    };
    let shown = key.0 .0.clone();
    let offer = Offer { format: OFFER_FORMAT.into(), v: OFFER_VERSION, key: key.0, presence_remote, from: machine.into(), replace };
    let mut line = serde_json::to_vec(&offer).map_err(|_| "pairing_failed")?;
    line.push(b'\n');
    drop(offer);
    // Whatever the target sends back is scrubbed of the key, in case it echoes it.
    let received = match run_there(remote, host, &["--pair-receive"], &line) {
        Ok(out) => answer(&out).map(|v| redact(v, &shown)).map_err(|e| format!("target: {}", e.replace(&shown, "[key]"))),
        Err(e) if e.starts_with("ssh_failed") => Err("ssh_failed".into()),
        Err(e) => Err(e),
    };
    line.iter_mut().for_each(|b| *b = 0);
    let received = received?;
    let here = door.call(&Request::SyncRoster)?;
    Ok(json!({
        "paired": true,
        "machine": target.get("machine").cloned().unwrap_or(Value::Null),
        "key": if key.1 { "generated" } else { "reused" },
        "here": roster_summary(&here),
        "target": received,
        "next": "Both computers share the roster with the same key, kept by each SlopBar and never shown. Check the target with get_machine_readiness (ssh_host), then ask the user to sit at the target's screen before run_on_machine sign_in_all with confirm: true.",
    }))
}

/// The target's half: reads the offer from `input`, points sharing at the source's rclone remote
/// when it has one, turns the roster on and saves the key. Never echoes the key. `saved` is the
/// target's own roster passphrase: a different one is replaced only when the offer says so, since
/// the target's other computers share it.
pub fn receive(door: &dyn Door, input: &mut dyn Read, saved: Option<Passphrase>) -> Result<Value, String> {
    let mut bytes = Vec::new();
    input.take(MAX_OFFER + 1).read_to_end(&mut bytes).map_err(|_| "pairing_unreadable")?;
    if bytes.len() as u64 > MAX_OFFER {
        bytes.iter_mut().for_each(|b| *b = 0);
        return Err("pairing_unreadable".into());
    }
    let offer: Result<Offer, _> = serde_json::from_slice(&bytes);
    bytes.iter_mut().for_each(|b| *b = 0);
    let offer = offer.map_err(|_| "pairing_unreadable")?;
    if offer.format != OFFER_FORMAT || offer.v != OFFER_VERSION {
        return Err("pairing_unsupported".into());
    }
    let replaced = saved.as_ref().is_some_and(|s| s.0 != offer.key.0);
    if replaced && !offer.replace {
        return Err("target_has_other_key".into());
    }
    let settings = door.call(&Request::GetSettings)?;
    let mut patch = serde_json::Map::new();
    match &offer.presence_remote {
        Some(name) => {
            if !lists_remote(&door.call(&Request::ListSharingLocations)?, name) {
                return Err(format!("remote_missing_here: {name}"));
            }
            if settings.get("presence_remote").and_then(Value::as_str) != Some(name) {
                patch.insert("presence_remote".into(), name.clone().into());
            }
        }
        None if settings.get("presence_remote").is_some_and(Value::is_string) || settings.get("presence_folder").is_some_and(Value::is_string) => {}
        None => return Err("no_sharing_location_here".into()),
    }
    if settings.get("account_roster") != Some(&Value::Bool(true)) {
        patch.insert("account_roster".into(), true.into());
    }
    if !patch.is_empty() {
        door.call(&Request::UpdateSettings { patch: Value::Object(patch) })?;
    }
    let roster = door.call(&Request::SetRosterPassphrase { passphrase: Some(offer.key) })?;
    Ok(json!({ "paired": true, "replaced_key": replaced, "from": offer.from.chars().filter(|c| !c.is_control()).take(63).collect::<String>(), "roster": roster_summary(&roster) }))
}

/// Whether list_sharing_locations' result shows rclone installed with the remote `name`.
fn lists_remote(places: &Value, name: &str) -> bool {
    places.pointer("/rclone/remotes").and_then(Value::as_array).is_some_and(|l| l.iter().any(|r| r.get("name").and_then(Value::as_str) == Some(name)))
}

/// `value` with every occurrence of `secret` in its strings replaced.
fn redact(value: Value, secret: &str) -> Value {
    match value {
        Value::String(s) if !secret.is_empty() && s.contains(secret) => Value::String(s.replace(secret, "[key]")),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| redact(v, secret)).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, redact(v, secret))).collect()),
        other => other,
    }
}

/// get_roster's result without the email list: machines, counts and what is waiting.
fn roster_summary(roster: &Value) -> Value {
    let pick = |k: &str| roster.get(k).cloned().unwrap_or(Value::Null);
    // The network's proxy's code, when it stopped the write or read: only a code SlopBar knows,
    // never other text another computer sends.
    let code = |k: &str| roster.get(k).and_then(Value::as_str).filter(|c| crate::net::PROXY_CODES.contains(c)).map_or(Value::Null, Value::from);
    json!({
        "enabled": pick("enabled"), "location": pick("location"), "passphrase_saved": pick("passphrase_saved"),
        "wrote": pick("wrote"), "write_error": code("write_error"), "read": pick("read"), "read_error": code("read_error"), "own_accounts": pick("own_accounts"),
        "machines": pick("machines"), "unreadable": pick("unreadable"),
        "waiting": roster.get("waiting").and_then(Value::as_array).map_or(0, Vec::len),
    })
}

// ---------------------------------------------------------------------------------------------
// Readiness

/// Whether this computer is ready for "Sign in all": what is missing, as codes, and what to do.
pub fn assess(running: bool, desktop: Option<crate::desktop::Desktop>, browser: bool, roster: &Value) -> (bool, Vec<&'static str>, String) {
    let mut problems = Vec::new();
    if !running {
        problems.push("app_not_running");
    }
    match desktop {
        None => problems.push("no_desktop_session"),
        Some(d) => problems.extend(d.problem()),
    }
    if !browser {
        problems.push("no_browser");
    }
    let ready = problems.is_empty();
    let flag = |k: &str| roster.get(k) == Some(&Value::Bool(true));
    if running {
        if !flag("enabled") { problems.push("roster_off"); }
        if !flag("location") { problems.push("no_sharing_location"); }
        if !flag("passphrase_saved") { problems.push("no_roster_key"); }
        if roster.get("unreadable").and_then(Value::as_u64).unwrap_or(0) > 0 { problems.push("roster_unreadable"); }
    }
    let next = match problems.first().copied() {
        Some("no_desktop_session") => "Nobody is signed in to this computer's desktop. The user signs in at its screen (or over Remote Desktop) and starts SlopBar there; sign-ins can only open on a desktop someone sees.",
        Some("app_not_running") => "SlopBar is not running on this computer's desktop. The user starts it there (Start menu); tools reached over SSH never start it, because it would run where nobody can see it.",
        Some("desktop_locked") => "This computer's desktop is locked: the user unlocks it at the screen before any sign-in starts.",
        Some("desktop_not_shown") => "This computer's desktop is signed in but not shown on any screen (a closed Remote Desktop, or another user switched in): the user goes back to it before any sign-in starts.",
        Some("no_browser") => "No Edge or Chrome was found for the sign-in pages: install one.",
        Some(_) => "Ready for sign-ins at the desktop, but the account roster is not set up: pair this computer with pair_machine from a computer that has the accounts.",
        None => "Ready: someone can complete sign-ins at this computer's screen. Ask them to sit there before sign_in_all with confirm: true.",
    };
    (ready, problems, next.into())
}

/// Readiness when the app does not answer: who is at the desktop, whether a browser and SlopBar
/// are installed. `reason` is the door's error.
pub fn readiness_without_app(reason: &str) -> Value {
    let desktop = crate::desktop::best(&crate::desktop::of_user());
    let browser = crate::signin::find_browser().map(|(k, _)| browser_name(k));
    let installed = std::env::var_os("LOCALAPPDATA").is_some_and(|d| std::path::Path::new(&d).join(r"SlopBar\slopbar.exe").is_file());
    let (ready, mut problems, mut next) = assess(false, desktop, browser.is_some(), &Value::Null);
    if reason == "unsupported_request" {
        problems.insert(0, "app_outdated");
        next = "The SlopBar running here predates readiness checks: update it, then ask again.".into();
    }
    let shell = ssh_shell();
    note_ssh_shell(shell, &mut problems, &mut next);
    json!({
        "app": { "running": reason == "unsupported_request", "installed": installed, "door": reason },
        "desktop": desktop, "browser": browser, "ssh_shell": shell, "ready_for_sign_in": ready && reason != "unsupported_request",
        "problems": problems, "next": next,
    })
}

/// Readiness's advice when sharing names an rclone remote that rclone here lacks.
pub fn rclone_missing_next(name: &str) -> String {
    format!("Sharing names the rclone remote {name}, but rclone here is missing or lacks it, so no roster is written or read: install rclone and create that remote (rclone config) at this computer's screen.")
}

/// Readiness's code, and its advice, when this computer's SSH sessions start Windows PowerShell.
const SSH_SHELL_WINDOWS_POWERSHELL: &str = "ssh_shell_windows_powershell";
const SSH_SHELL_ADVICE: &str = "SSH starts Windows PowerShell here, which can outlive sessions: set OpenSSH's DefaultShell to pwsh or remove it.";

/// Notes this computer's SSH shell in readiness: Windows PowerShell is a problem of its own, noted
/// after whether sign-ins can start was decided, so it never stops them.
pub fn note_ssh_shell(shell: Option<&str>, problems: &mut Vec<&'static str>, next: &mut String) {
    if shell == Some("windows_powershell") {
        problems.push(SSH_SHELL_WINDOWS_POWERSHELL);
        next.push(' ');
        next.push_str(SSH_SHELL_ADVICE);
    }
}

/// The shell this computer's OpenSSH server starts for every SSH command, as it picks it
/// (Win32-OpenSSH's `set_defaultshell`): the raw DefaultShell value under HKLM\SOFTWARE\OpenSSH in
/// the 64-bit view, cmd when it is missing or empty. Only its kind is told (`shell_kind`), never
/// the path: it can name a user, and any administrator there can write it. None without an OpenSSH
/// server, or when it cannot be read. It may touch the file system: keep it off async threads.
pub fn ssh_shell() -> Option<&'static str> {
    #[cfg(windows)]
    {
        registry::hklm_string(r"SYSTEM\CurrentControlSet\Services\sshd", "ImagePath").ok().flatten()?;
        let value = registry::hklm_string(r"SOFTWARE\OpenSSH", "DefaultShell").ok()?;
        Some(shell_kind(value.as_deref(), is_file_there))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// What DefaultShell's value names, as sshd starts it: missing or empty is cmd. sshd quotes the
/// value itself and starts it as a path, never from PATH, so a quoted, relative or `%VAR%` value,
/// or a path to no file, starts nothing at all (`missing`); a path without an extension starts its
/// `.exe`. A path from a drive letter, or rooted at `\` or `/` (the current drive, or a share), is
/// a path. Otherwise the program's file name tells pwsh, windows_powershell, cmd or other.
/// `exists` says whether a file (not a folder) is there; it sees a Store app's alias as one.
#[cfg_attr(not(windows), allow(dead_code))]
fn shell_kind(value: Option<&str>, exists: impl Fn(&str) -> bool) -> &'static str {
    let Some(value) = value.filter(|v| !v.is_empty()) else { return "cmd" };
    let bytes = value.as_bytes();
    let absolute = (bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/')) || matches!(bytes[0], b'\\' | b'/');
    let name = value.rsplit(['\\', '/']).next().unwrap_or(value).trim_end().to_ascii_lowercase();
    let bare = !name.contains('.');
    if !absolute || value.contains('"') || !(exists(value) || (bare && exists(&format!("{value}.exe")))) {
        return "missing";
    }
    match name.strip_suffix(".exe").unwrap_or(&name) {
        "pwsh" => "pwsh",
        "powershell" => "windows_powershell",
        "cmd" => "cmd",
        _ => "other",
    }
}

/// Whether a file (not a folder) is at `path`, without following it: a Store app's alias counts.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_file_there(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_dir())
}

/// A registry string as sshd reads it, a C string: up to the first NUL.
#[cfg_attr(not(windows), allow(dead_code))]
fn c_string(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

#[cfg(windows)]
mod registry {
    /// A string value under HKEY_LOCAL_MACHINE as sshd reads DefaultShell: the 64-bit view, with no
    /// %VAR% expansion. None when the key or the value is missing.
    pub fn hklm_string(key: &str, value: &str) -> Result<Option<String>, u32> {
        use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
        use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY};
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (key, value) = (wide(key), wide(value));
        let mut data = vec![0u16; 1024];
        let mut size = (data.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND | RRF_SUBKEY_WOW6464KEY,
                std::ptr::null_mut(), data.as_mut_ptr().cast(), &mut size)
        };
        match status {
            ERROR_SUCCESS => Ok(Some(super::c_string(&data[..(size as usize / 2).min(data.len())]))),
            ERROR_FILE_NOT_FOUND => Ok(None),
            error => Err(error),
        }
    }
}

pub fn browser_name(kind: crate::signin::Kind) -> &'static str {
    match kind {
        crate::signin::Kind::Chrome => "chrome",
        crate::signin::Kind::Edge => "edge",
    }
}

// ---------------------------------------------------------------------------------------------
// The MCP tools served by `slopbar-mcp` itself (see `mcp::step`)

/// Whether `run_on_machine` may run `tool` with `arguments` on another computer.
pub fn check_remote(tool: &str, arguments: &Value) -> Result<(), String> {
    if !REMOTE_TOOLS.contains(&tool) {
        return Err(format!("run_on_machine runs only {}", REMOTE_TOOLS.join(", ")));
    }
    if tool == "update_settings" {
        let keys = arguments.pointer("/patch").and_then(Value::as_object).ok_or("Invalid arguments: update_settings takes a patch")?;
        if let Some(k) = keys.keys().find(|k| !REMOTE_SETTINGS.contains(&k.as_str())) {
            return Err(format!("run_on_machine changes only {} on another computer, not {k}", REMOTE_SETTINGS.join(", ")));
        }
    }
    Ok(())
}

/// How long `setup_sharing` with `ssh_host` waits for another computer to get rclone and start its
/// Dropbox sign-in. The setup goes on there after that; calling again joins it.
const SETUP_START: Duration = Duration::from_secs(150);

/// What `setup_sharing` answers without `confirm: true`. Short: every string a tool returns is cut
/// at `mcp::MAX_TEXT`, and this one ends with when to confirm.
const SETUP_SHARING_PREVIEW: &str = "Nothing was started. confirm: true gets rclone unless a recent one is installed (a fixed, checksum-verified download from rclone's site into SlopBar's folder), opens Dropbox's consent page in the default browser (with ssh_host, that computer's) for the user to sign in and click Allow themselves, and turns in-use sharing on through rclone, whose access covers the whole Dropbox; SlopBar uses only its SlopBar folder there. Confirm only if the user asked for this setup.";

/// What `setup_sharing` with `ssh_host` says to do next (each fits `mcp::MAX_TEXT`, like the preview).
/// The page opens on that computer's screen:
const SETUP_THERE: &str = "That computer's SlopBar gets rclone if needed and opens Dropbox's page in its own browser, on its screen, with a notification there. The user signs in there with the Dropbox account their other computers use and clicks Allow; follow it with get_sharing_setup with this ssh_host, wait_seconds: 45 and since: the last revision, until phase is done or failed.";
/// The setup joined there was started with `open_here`, and this call has none, so nothing opens:
const SETUP_ELSEWHERE: &str = "That setup finishes its Dropbox sign-in through a tunnel, in a browser on another computer, so nothing opens on that computer's screen. If the user wants the page on this computer, call setup_sharing again with this ssh_host, open_here: true and confirm: true; otherwise follow it with get_sharing_setup with this ssh_host, wait_seconds: 45 and since: the last revision.";
/// The page is ready here:
const SETUP_HERE: &str = "Dropbox's page is ready in this computer's browser: opened says whether it opened by itself, notified whether a notification here offers it (a page opened while nobody uses SlopBar can stay behind other windows, so tell the user where to look). The user signs in with the Dropbox account their other computers use and clicks Allow on dropbox.com; then call get_sharing_setup with this ssh_host, wait_seconds: 45 and since: the last revision, until phase is done or failed.";
/// Nothing could be shown here:
const SETUP_NOT_SHOWN: &str = "Nothing was shown here: the user turned off both opening Dropbox's page (sharing_open_browser) and its notification (notifications.sharing_sign_in). Ask the user before turning one on with update_settings, then call setup_sharing again with this ssh_host, open_here: true and confirm: true (it joins the same setup).";
/// Still getting rclone there when this call stops waiting:
const SETUP_STILL_GOING: &str = "Still under way on that computer. Call setup_sharing again with this ssh_host, open_here: true and confirm: true to keep waiting (it joins the same setup), or with cancel: true to stop it.";

/// How long `--sharing-state` waits on the other computer for its sign-in before answering, in
/// seconds: each ask over SSH starts a shell there, so it asks seldom and the waiting is done there.
const STATE_WAIT: &str = "40";

/// Whether a setup's phase (in `get_sharing_setup`'s or `--sharing-state`'s answer) is under way.
pub(crate) fn setup_running(setup: &Value) -> bool {
    setup.get("phase").and_then(Value::as_str).is_some_and(|p| matches!(p, "rclone" | "dropbox" | "sharing"))
}

/// The rclone remote this computer shares through, when it is a Dropbox one: the name another
/// computer's setup takes, since pairing needs the same name on both.
fn own_dropbox_remote(door: &dyn Door) -> Option<String> {
    let places = door.call(&Request::ListSharingLocations).ok()?;
    let name = places.pointer("/current/presence_remote")?.as_str()?;
    let remotes = places.pointer("/rclone/remotes")?.as_array()?;
    remotes.iter().any(|r| r["name"] == name && r["kind"] == "dropbox").then(|| name.to_string())
}

/// `setup_sharing` for another computer. By default that computer's SlopBar runs the setup as if it
/// was started at its screen: Dropbox's page opens in its own browser, with its notification, and
/// it is refused while nobody can see that desktop (the owner's ruling: the page opens on the
/// computer being set up). Only with `open_here` (that computer's screen is out of reach) does its
/// SlopBar start rclone's sign-in without a browser (`via_tunnel`), while this computer's app opens
/// an SSH tunnel to its rclone and this computer's browser on the link, built here from the state
/// value alone: Dropbox's answer reaches that computer's rclone through the tunnel, no token passes
/// here, and the state value never appears in a result. `get_sharing_setup` with `ssh_host`
/// follows either. A call made while a setup runs there joins it, starting nothing: with
/// `open_here` it opens the tunnel and browser here for it as for its own, though a tunnel that
/// fails stops only a setup this call started; without, nothing opens here, and a setup waiting for
/// a tunnel says how to finish it here.
fn setup_remote_sharing(door: &dyn Door, ssh: &dyn Remote, host: &str, args: &Value) -> Result<Value, String> {
    let cancel_there = || remote_call(ssh, host, "setup_sharing", &json!({ "cancel": true })).map_err(from_there);
    let close_here = || door.call(&Request::SharingTunnel { host: host.into(), state: None, close: true });
    if args.get("cancel").and_then(Value::as_bool) == Some(true) {
        let _ = close_here();
        return cancel_there();
    }
    // The user's go-ahead given here covers that computer's part.
    let open_here = args.get("open_here").and_then(Value::as_bool) == Some(true);
    let mut start = json!({ "confirm": true });
    if open_here {
        start["via_tunnel"] = true.into();
    }
    let remote = args.get("remote").and_then(Value::as_str).map(str::to_owned).or_else(|| own_dropbox_remote(door));
    if let Some(remote) = remote {
        start["remote"] = remote.into();
    }
    if args.get("reconnect").and_then(Value::as_bool) == Some(true) {
        start["reconnect"] = true.into();
    }
    let (there, joined) = match remote_call(ssh, host, "setup_sharing", &start) {
        Ok(state) => (state, false),
        // One already runs there (this call came again while it got rclone): this one joins it.
        Err(e) if e.is("sharing_setup_busy") => {
            let there = remote_call(ssh, host, "get_sharing_setup", &json!({})).map_err(from_there)?;
            if !setup_running(&there) {
                // It ended meanwhile: its own state says how.
                return Ok(there);
            }
            (there, true)
        }
        // A SlopBar from before the guided setup: unknown tools are refused in these words.
        Err(e) if e.text.contains("nknown tool") => return Err("update_slopbar_there".into()),
        Err(e) => return Err(from_there(e)),
    };
    if !open_here {
        // Nothing opens here without open_here: a setup joined there that was started with it
        // (whose page opens only through a tunnel) says how to finish it here instead.
        let next = if there["via_tunnel"] == true { SETUP_ELSEWHERE } else { SETUP_THERE };
        return Ok(with_next(there, next));
    }
    let deadline = std::time::Instant::now() + SETUP_START;
    loop {
        // Its errors are that computer's, as are those of the calls below (its door's own codes,
        // which this computer would otherwise word as its own).
        let link = run_there(ssh, host, &["--sharing-state", STATE_WAIT], b"").and_then(|out| answer(&out)).map_err(from_there)?;
        if let Some(state) = link.get("state").and_then(Value::as_str) {
            let shown = match door.call(&Request::SharingTunnel { host: host.into(), state: Some(state.into()), close: false }) {
                Ok(shown) => shown,
                // A setup this call joined is not its own to stop: it goes on there.
                Err(e) if joined => return Err(if e == "tunnel_failed" { "tunnel_failed_joined".into() } else { e }),
                Err(e) => {
                    let _ = cancel_there();
                    return Err(e);
                }
            };
            let (opened, notified) = (shown["opened"] == true, shown["notified"] == true);
            let next = if opened || notified { SETUP_HERE } else { SETUP_NOT_SHOWN };
            return Ok(json!({ "phase": "dropbox", "browser": "this computer", "opened": opened, "notified": notified, "next": next }));
        }
        if !setup_running(&link) {
            // Already connected (no sign-in needed), or it stopped: its own state says which.
            return remote_call(ssh, host, "get_sharing_setup", &json!({})).map_err(from_there);
        }
        if std::time::Instant::now() > deadline {
            // Still getting rclone there (a slow link): it goes on, and is joined by calling again.
            return Ok(with_next(remote_call(ssh, host, "get_sharing_setup", &json!({})).map_err(from_there)?, SETUP_STILL_GOING));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// An error met on another computer, said to be that computer's (`mcp::explain` words "target: …"
/// as "On the other computer: …").
fn from_there(error: impl std::fmt::Display) -> String {
    format!("target: {error}")
}

/// Another computer's answer with what to do next added; one that is no object (only a broken or
/// misbehaving computer gives one) is passed on as it is.
fn with_next(mut answer: Value, next: &str) -> Value {
    if let Some(fields) = answer.as_object_mut() {
        fields.insert("next".into(), next.into());
    }
    answer
}

/// Marks whether another computer's finished setup sees this one, which shares too, among the
/// computers sharing there: when it does not, it may be signed in to another Dropbox account.
fn seen_from_there(answer: &mut Value, me: &str) {
    let seen = answer.get("found").and_then(Value::as_array)
        .is_some_and(|found| found.iter().filter_map(Value::as_str).any(|name| name.eq_ignore_ascii_case(me)));
    answer["sees_this_computer"] = seen.into();
    if !seen {
        answer["next"] = json!("That computer does not see this one among the computers sharing there, so it may be signed in to another Dropbox account. Run setup_sharing with this ssh_host, reconnect: true and confirm: true, and have the user sign in with the Dropbox account this computer uses. A computer that has just started sharing can take a minute to appear.");
    }
}

/// `get_machine_readiness`, `pair_machine`, `run_on_machine`, `setup_sharing` and
/// `get_sharing_setup`. `remote_ok` is false inside a `--call` reached over SSH, so a target never
/// reaches on to a third computer.
pub fn local_tool(name: &str, args: &Value, pipe: &str, remote_ok: bool) -> Result<Value, String> {
    let host = args.get("ssh_host").and_then(Value::as_str);
    if host.is_some() && !remote_ok {
        return Err("not_from_a_remote_call".into());
    }
    let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);
    match (name, host) {
        ("get_machine_readiness", Some(host)) => {
            // Waiting for a change is passed on: the other computer's app does the waiting.
            let wait: serde_json::Map<String, Value> = args.as_object().into_iter().flatten()
                .filter(|(k, _)| matches!(k.as_str(), "wait_seconds" | "since")).map(|(k, v)| (k.clone(), v.clone())).collect();
            let answer = remote_readiness(&Ssh, host, &Value::Object(wait))?;
            remember_answer(host, &answer);
            Ok(answer)
        }
        ("get_machine_readiness", None) => match crate::door::call(pipe, &Request::GetReadiness {
            wait_seconds: args.get("wait_seconds").and_then(Value::as_u64).map(|s| s.min(45) as u32),
            since: args.get("since").and_then(Value::as_u64),
        }) {
            Ok(value) => Ok(value),
            Err(e) if matches!(e.as_str(), "not_running" | "unsupported_request" | "door_unavailable" | "timeout") => Ok(readiness_without_app(&e)),
            Err(e) => Err(e),
        },
        ("pair_machine", Some(host)) => {
            let saved = crate::settings::app_dir().and_then(|d| crate::roster::load_passphrase(&d));
            let machine = crate::presence::identity().map(|(_, name)| name.to_string()).unwrap_or_default();
            let replace = args.get("replace_key").and_then(Value::as_bool).unwrap_or(false);
            let paired = pair(&LocalDoor(pipe.into()), &Ssh, host, saved, &machine, replace)?;
            remember_answer(host, &paired);
            Ok(paired)
        }
        // Nothing starts (no download, no browser) before the user's go-ahead: it says what would.
        ("setup_sharing", _) if !(flag("confirm") || flag("cancel")) => Ok(json!({ "started": false, "next": SETUP_SHARING_PREVIEW })),
        ("setup_sharing", Some(host)) => setup_remote_sharing(&LocalDoor(pipe.into()), &Ssh, host, args),
        ("setup_sharing", None) => crate::door::call(pipe, &Request::SetupSharing {
            remote: args.get("remote").and_then(Value::as_str).map(String::from),
            via_tunnel: flag("via_tunnel"),
            cancel: flag("cancel"),
            reconnect: flag("reconnect"),
        }),
        ("get_sharing_setup", Some(host)) => {
            // Waiting for a change is passed on: the other computer's app does the waiting.
            let wait: serde_json::Map<String, Value> = args.as_object().into_iter().flatten()
                .filter(|(k, _)| matches!(k.as_str(), "wait_seconds" | "since")).map(|(k, v)| (k.clone(), v.clone())).collect();
            let mut answer = remote_call(&Ssh, host, "get_sharing_setup", &Value::Object(wait))?;
            if !setup_running(&answer) {
                let _ = crate::door::call(pipe, &Request::SharingTunnel { host: host.into(), state: None, close: true });
            }
            let shares_here = crate::door::call(pipe, &Request::GetSettings)
                .is_ok_and(|s| s["presence_remote"].is_string() || s["presence_folder"].is_string());
            if answer["phase"] == "done" && shares_here {
                if let Some((_, me)) = crate::presence::identity() {
                    seen_from_there(&mut answer, me);
                }
            }
            Ok(answer)
        }
        ("get_sharing_setup", None) => crate::door::call(pipe, &Request::GetSharingSetup {
            wait_seconds: args.get("wait_seconds").and_then(Value::as_u64).map(|s| s.min(45) as u32),
            since: args.get("since").and_then(Value::as_u64),
        }),
        ("run_on_machine", Some(host)) => {
            let tool = args.get("tool").and_then(Value::as_str).unwrap_or("");
            let arguments = args.get("arguments").cloned().unwrap_or(json!({}));
            check_remote(tool, &arguments)?;
            if tool != "get_machine_readiness" {
                return remote_call(&Ssh, host, tool, &arguments).map_err(String::from);
            }
            let answer = remote_readiness(&Ssh, host, &arguments)?;
            remember_answer(host, &answer);
            Ok(answer)
        }
        _ => Err("Invalid arguments: ssh_host is required".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const KEY_SHOWN: &str = "slopbar-pair-";

    #[test]
    fn keys_are_long_random_and_pass_as_roster_passphrases() {
        let (a, b) = (generate_key(), generate_key());
        assert_ne!(a.0, b.0);
        assert_eq!(a.0.len(), KEY_PREFIX.len() + 52, "256 bits in base32");
        assert!(a.0[KEY_PREFIX.len()..].chars().all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c)));
        assert!(!format!("{a:?}").contains(KEY_SHOWN), "debug output hides the key");
        let dir = crate::test_support::TempDir::new();
        crate::roster::store_passphrase(&dir.0, Some(&a)).unwrap();
        assert_eq!(crate::roster::load_passphrase(&dir.0).map(|p| p.0), Some(a.0));
        assert_eq!(base32(b"foobar"), "mzxw6ytboi", "RFC 4648, lowercase, unpadded");
    }

    #[test]
    fn hosts_are_never_read_as_ssh_options() {
        for good in ["box", "laptop-2", "user@laptop-2", "box.tail1234.ts.net", "100.64.0.7", "fe80::1"] {
            assert!(valid_host(good), "{good}");
        }
        for bad in ["", "-oProxyCommand=calc", "a b", "a;b", "a@b@c", "user@", "a\nb", "$(x)", "a`b", &"x".repeat(254)] {
            assert!(!valid_host(bad), "{bad:?}");
        }
    }

    /// The script a remote command carries, decoded.
    fn script_of(cmd: &str) -> String {
        let b64 = cmd.rsplit(' ').next().unwrap();
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        let units: Vec<u16> = bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16(&units).unwrap()
    }

    /// The shell a remote command names, and the arguments its script gives slopbar-mcp.
    fn parts(cmd: &str) -> (String, Vec<String>) {
        let script = script_of(cmd);
        let line = script.lines().find_map(|l| l.strip_prefix("$out = & $exe ")).expect("runs slopbar-mcp");
        let mut quoted = line.strip_suffix(" | ForEach-Object { $_ }").unwrap().chars().peekable();
        let mut args = Vec::new();
        while let Some(c) = quoted.next() {
            if c != '\'' {
                continue; // the space between two arguments
            }
            let mut arg = String::new();
            loop {
                match quoted.next() {
                    Some('\'') if quoted.peek() == Some(&'\'') => {
                        quoted.next();
                        arg.push('\'');
                    }
                    Some('\'') | None => break,
                    Some(c) => arg.push(c),
                }
            }
            args.push(arg);
        }
        (cmd.split(' ').next().unwrap().to_string(), args)
    }

    /// The tool and arguments a `--call-b64` asks for.
    fn tool_asked(args: &[String]) -> Option<(String, Value)> {
        let [flag, b64] = args else { return None };
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b64).ok()?;
        let ask: Value = serde_json::from_slice(&bytes).ok()?;
        (flag == "--call-b64").then(|| (ask["tool"].as_str().unwrap_or_default().to_string(), ask["arguments"].clone()))
    }

    /// What a command asks of slopbar-mcp: the tool for `--call-b64`, else its arguments.
    fn asked_of(cmd: &str) -> String {
        let (_, args) = parts(cmd);
        tool_asked(&args).map_or_else(|| args.join(" "), |(tool, _)| tool)
    }

    #[test]
    fn the_remote_command_carries_no_secret_and_quotes_its_arguments() {
        let cmd = remote_command(&["--call-b64", "it's"]);
        assert!(cmd.starts_with("pwsh -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand "), "PowerShell 7 first: {cmd}");
        let script = script_of(&cmd);
        assert!(script.starts_with("'slopbar-remote: started'\n$ErrorActionPreference = 'Stop'\n"), "it says it started before anything can fail: {script}");
        assert!(script.contains("$out = & $exe '--call-b64' 'it''s' | ForEach-Object { $_ }"), "{script}");
        assert!(script.contains("SlopBar\\slopbar-mcp.exe"));
        assert_eq!(parts(&cmd), ("pwsh".to_string(), vec!["--call-b64".to_string(), "it's".to_string()]));
        let fallback = fallback_command(&["--call-b64", "it's"]);
        assert!(fallback.starts_with("powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand "), "{fallback}");
        assert_eq!(script_of(&fallback), script, "the same script under Windows PowerShell");
        // Nothing any remote shell (cmd, Windows PowerShell, PowerShell 7) would read as quoting or
        // an operator: a program name, its switches and base64. The arguments are only in the script.
        for c in [&cmd, &fallback] {
            assert!(c.chars().all(|ch| ch.is_ascii_alphanumeric() || " -+/=".contains(ch)), "{c}");
            assert!(!c.contains("call-b64"), "{c}");
        }
    }

    #[test]
    fn ssh_hands_back_the_output_or_its_own_failure() {
        let shell_said = b"'pwsh' is not recognized as an internal or external command,\r\n";
        assert_eq!(finished(Vec::new(), Some(1), shell_said).unwrap(), b"", "the remote shell's own failure is the command's");
        assert_eq!(finished(b"{\"ok\":{}}\r\n".to_vec(), Some(0), b"").unwrap(), b"{\"ok\":{}}\r\n");
        let refused = b"** WARNING: connection is not using a post-quantum key exchange algorithm.\r\n** This session may be vulnerable.\r\nssh: connect to host box port 22: Connection refused\r\n";
        assert_eq!(finished(Vec::new(), Some(255), refused).unwrap_err(), "ssh_failed: ssh: connect to host box port 22: Connection refused");
    }

    /// Another computer's shell, answering each command with the next of `replies`.
    struct Scripted {
        replies: RefCell<Vec<Result<Vec<u8>, String>>>,
        commands: RefCell<Vec<String>>,
        inputs: RefCell<Vec<Vec<u8>>>,
    }

    impl Remote for Scripted {
        fn run(&self, _host: &str, command: &str, input: &[u8]) -> Result<Vec<u8>, String> {
            self.commands.borrow_mut().push(command.into());
            self.inputs.borrow_mut().push(input.to_vec());
            let mut replies = self.replies.borrow_mut();
            assert!(!replies.is_empty(), "tried again: {:?}", parts(command));
            replies.remove(0)
        }
    }

    fn scripted(replies: Vec<Result<Vec<u8>, String>>) -> Scripted {
        Scripted { replies: RefCell::new(replies), commands: RefCell::default(), inputs: RefCell::default() }
    }

    fn printed(out: &str) -> Result<Vec<u8>, String> {
        Ok(out.as_bytes().to_vec())
    }

    #[test]
    fn windows_powershell_runs_only_where_powershell_7_never_started_the_script() {
        let args = ["--call-b64", "it's"];
        let asked = vec!["--call-b64".to_string(), "it's".to_string()];
        let tried = |there: &Scripted| there.commands.borrow().iter().map(|c| parts(c)).collect::<Vec<_>>();
        let started = |rest: &str| format!("{STARTED}\r\n{rest}");
        let answer = started("{\"ok\":{\"paired\":true}}\r\n");
        // pwsh never started the script (none there, or it cannot start): the same script and
        // input go to Windows PowerShell, whatever the shell printed of its own (cmd's AutoRun, a
        // profile's greeting) and whatever exit code it gave.
        for first in ["", "\r\n", "Active code page: 65001\r\n", "Welcome back\r\n"] {
            let there = scripted(vec![printed(first), printed(&answer)]);
            assert_eq!(run_there(&there, "box", &args, b"offer\n").unwrap(), answer.as_bytes(), "{first:?}");
            assert_eq!(tried(&there), [("pwsh".to_string(), asked.clone()), ("powershell".to_string(), asked.clone())]);
            assert_eq!(*there.inputs.borrow(), [b"offer\n".to_vec(), b"offer\n".to_vec()]);
        }
        // The second try's own failure is the outcome.
        let there = scripted(vec![printed(""), Err("ssh_timeout".into())]);
        assert_eq!(run_there(&there, "box", &args, b"").unwrap_err(), "ssh_timeout");

        // Once the script started under pwsh, its outcome stands, whatever came after: an answer, an
        // error answer, SlopBar missing there, or nothing more (slopbar-mcp crashed or was killed,
        // which a PowerShell remote shell reports as exit 1, like a missing pwsh). The shell's own
        // text before the marker counts for nothing, with or without a line break.
        for before in ["", "Welcome back\r\n", "Hi ", "noise", "\u{1b}]0;title\u{7}"] {
            for rest in ["{\"ok\":{\"paired\":true}}\r\n", "{\"error\":\"target_has_other_key\"}\r\n", "{\"error\":\"slopbar_not_installed\"}\r\n", ""] {
                let there = scripted(vec![printed(&format!("{before}{}", started(rest)))]);
                run_there(&there, "box", &args, b"").unwrap();
                assert_eq!(tried(&there), [("pwsh".to_string(), asked.clone())], "never twice: {before:?} {rest:?}");
            }
        }
        let glued = scripted(vec![printed(&format!("Hi {}", started("{\"ok\":{\"paired\":true}}\r\n")))]);
        assert_eq!(remote_call(&glued, "box", "get_roster", &json!({})).unwrap(), json!({ "paired": true }), "read once, from pwsh");
        let there = scripted(vec![printed(&started(""))]);
        assert_eq!(remote_call(&there, "box", "sign_in_all", &json!({ "confirm": true })).unwrap_err().text, "remote_no_answer");
        // SSH itself failed or timed out: nothing is tried again.
        for e in ["ssh_failed: Connection refused", "ssh_timeout"] {
            let there = scripted(vec![Err(e.into())]);
            assert_eq!(run_there(&there, "box", &args, b"").unwrap_err(), e);
            assert_eq!(there.commands.borrow().len(), 1);
        }
        // Neither PowerShell there: no answer, as before.
        let there = scripted(vec![printed(""), printed("")]);
        assert_eq!(remote_call(&there, "box", "get_app_info", &json!({})).unwrap_err().text, "remote_no_answer");
        assert_eq!(tried(&there).iter().map(|(shell, _)| shell.as_str()).collect::<Vec<_>>(), ["pwsh", "powershell"]);
    }

    /// Whether another computer's shell runs `command`'s script: with PowerShell 7 there, only
    /// `pwsh` is ever asked for; without it, `pwsh` is a program its shell cannot find.
    fn shell_runs(pwsh: bool, command: &str) -> bool {
        match (pwsh, parts(command).0.as_str()) {
            (true, "pwsh") | (false, "powershell") => true,
            (false, "pwsh") => false,
            (_, shell) => panic!("{shell} asked for where PowerShell 7 answers"),
        }
    }

    /// What the target's shell prints on standard output for a program it cannot find: nothing
    /// (cmd's "'pwsh' is not recognized…" goes to standard error).
    fn not_found() -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    #[test]
    fn answers_are_ascii_and_read_back() {
        let v = json!({ "ok": { "email": "zoë@example.invalid", "machine": "東京" } });
        let text = ascii_json(&v);
        assert!(text.is_ascii());
        assert_eq!(answer(format!("noise\r\n{text}\r\n").as_bytes()).unwrap(), v["ok"]);
        assert_eq!(answer(b"{\"error\":\"desktop_locked\"}").unwrap_err(), "desktop_locked");
        assert_eq!(answer(b"").unwrap_err(), "remote_no_answer");
    }

    /// What every SlopBar with the guided setup printed for its busy refusal before `code` came
    /// (mcp.rs, unchanged from 5147f15 until then). A fact about released builds: never reword it.
    const BUSY_BEFORE_CODES: &str = "A sharing setup is already running on this computer: follow it with get_sharing_setup, or stop it with setup_sharing cancel: true.";

    /// `answered`'s bytes, for `read`.
    fn printed_line(line: &Value) -> Vec<u8> {
        answered(line).unwrap()
    }

    #[test]
    fn an_error_from_another_computer_is_known_by_its_code_whatever_its_words() {
        // A current SlopBar prints the words an agent reads, with the code beside them.
        for code in ["sharing_setup_busy", "desktop_locked", "desktop_not_shown", "signin_busy", "CLI not found", "not_running", "app_outdated", "target_has_other_key"] {
            let error = read(&printed_line(&crate::mcp::error_line(code))).unwrap_err();
            assert!(error.is(code), "{error:?}");
            assert_eq!(error.text, crate::mcp::explain(code), "an agent reads the same words as before");
            assert_ne!(error.text, code);
            assert_eq!(String::from(error), crate::mcp::explain(code), "and so do agents and Settings here");
        }
        // An earlier one printed only the words: the setup's busy refusal is known by them.
        let earlier = read(&printed_line(&json!({ "error": BUSY_BEFORE_CODES }))).unwrap_err();
        assert!(earlier.is("sharing_setup_busy"), "{earlier:?}");
        assert_eq!(earlier.to_string(), BUSY_BEFORE_CODES);
        assert_eq!(EARLIER_WORDS.map(|(_, words)| words), [BUSY_BEFORE_CODES]);
        // Other words stay words, with no code; a code `explain` leaves as it is stays a code.
        for words in ["Unknown tool: setup_sharing", "Invalid arguments: confirm is required", BUSY_BEFORE_CODES.trim_end_matches('.')] {
            assert_eq!(read(&printed_line(&json!({ "error": words }))).unwrap_err(), RemoteError { code: None, text: words.into() });
        }
        assert!(read(&printed_line(&json!({ "error": "monitor_reconnect" }))).unwrap_err().is("monitor_reconnect"));
        // A `code` that is not one is ignored.
        for odd in [json!("sharing_setup_busy: x"), json!("x".repeat(65)), json!(7), Value::Null] {
            assert_eq!(read(&printed_line(&json!({ "error": "Words.", "code": odd }))).unwrap_err().code, None, "{odd}");
        }
        // Met on the way here: already a code, or a code with ssh's reason.
        assert!(RemoteError::here("ssh_timeout".into()).is("ssh_timeout"));
        assert_eq!(RemoteError::here("ssh_failed: Connection refused".into()).code, None);
        assert!(read(b"").unwrap_err().is("remote_no_answer"));
    }

    /// A fake app: records requests, answers settings and roster calls.
    struct FakeApp {
        settings: Value,
        saved: Option<&'static str>,
        remotes: Vec<&'static str>,
        /// Opening the tunnel for a sign-in fails with this (`SharingTunnel`).
        tunnel_error: Option<&'static str>,
        seen: RefCell<Vec<Request>>,
    }

    impl FakeApp {
        fn new(settings: Value) -> Self { FakeApp { settings, saved: None, remotes: vec!["dropbox"], tunnel_error: None, seen: RefCell::default() } }
        fn key_set(&self) -> Option<String> {
            self.seen.borrow().iter().find_map(|r| match r { Request::SetRosterPassphrase { passphrase: Some(p) } => Some(p.0.clone()), _ => None })
        }
        fn patches(&self) -> Vec<Value> {
            self.seen.borrow().iter().filter_map(|r| match r { Request::UpdateSettings { patch } => Some(patch.clone()), _ => None }).collect()
        }
    }

    impl Door for FakeApp {
        fn call(&self, request: &Request) -> Result<Value, String> {
            self.seen.borrow_mut().push(request.clone());
            if let (Request::SharingTunnel { close: false, .. }, Some(error)) = (request, self.tunnel_error) {
                return Err(error.into());
            }
            Ok(match request {
                Request::GetSettings => self.settings.clone(),
                Request::ListSharingLocations => json!({
                    "rclone": { "installed": true, "remotes": self.remotes.iter().map(|n| json!({ "name": n, "kind": if n.contains("drive") { "drive" } else { "dropbox" } })).collect::<Vec<_>>() },
                    "current": { "presence_remote": self.settings["presence_remote"] },
                }),
                Request::SetRosterPassphrase { .. } | Request::SyncRoster => json!({ "enabled": true, "location": true, "passphrase_saved": true, "machines": [{ "machine": "OTHER", "accounts": 3 }], "waiting": [{ "provider": "claude", "id": "a", "email": "a@example.invalid" }] }),
                _ => json!({}),
            })
        }
    }

    /// A fake other computer: its app answers readiness, and `--pair-receive` runs `receive`
    /// against `target` with the bytes ssh would carry.
    struct FakeSsh<'a> {
        target: &'a FakeApp,
        running: bool,
        /// A misbehaving target that echoes what it was sent.
        echo: bool,
        /// A misbehaving target that refuses with the key it was sent.
        echo_refusal: bool,
        /// It has PowerShell 7; without it, its shell finds no `pwsh` and only Windows PowerShell
        /// runs the script.
        pwsh: bool,
        commands: RefCell<Vec<String>>,
        inputs: RefCell<Vec<Vec<u8>>>,
    }

    impl Remote for FakeSsh<'_> {
        fn run(&self, _host: &str, command: &str, input: &[u8]) -> Result<Vec<u8>, String> {
            self.commands.borrow_mut().push(command.into());
            self.inputs.borrow_mut().push(input.to_vec());
            if !shell_runs(self.pwsh, command) {
                return not_found();
            }
            let (_, args) = parts(command);
            let out = match tool_asked(&args) {
                None if args == ["--pair-receive"] => match receive(self.target, &mut &input[..], self.target.saved.map(|s| Passphrase(s.into()))) {
                    // A misbehaving target that refuses with the key it was sent, as words and code.
                    Ok(_) if self.echo_refusal => {
                        let key = serde_json::from_slice::<Value>(input).unwrap()["key"].clone();
                        json!({ "error": key, "code": key })
                    }
                    Ok(v) => json!({ "ok": if self.echo { json!({ "leak": String::from_utf8_lossy(input) }) } else { v } }),
                    // As a real target prints it (`mcp::pair_receive`).
                    Err(e) => crate::mcp::error_line(&e),
                },
                Some((tool, _)) if tool == "list_sharing_locations" => {
                    let mut places = self.target.call(&Request::ListSharingLocations).unwrap();
                    places["current"] = json!({ "presence_remote": self.target.settings["presence_remote"] });
                    json!({ "ok": places })
                }
                Some((tool, _)) if tool == "get_machine_readiness" => {
                    let location = self.target.settings["presence_remote"].is_string() || self.target.settings["presence_folder"].is_string();
                    json!({ "ok": { "app": { "running": self.running }, "roster": { "location": location } } })
                }
                _ => panic!("not asked of this computer: {args:?}"),
            };
            answered(&out)
        }
    }

    /// What the script prints when slopbar-mcp answers `out` (see `mcp::print_answer`).
    fn answered(out: &Value) -> Result<Vec<u8>, String> {
        Ok(format!("{STARTED}\r\n{}\r\n", ascii_json(out)).into_bytes())
    }

    /// Another computer running a sharing setup: its tools and `--sharing-state` answer as scripted,
    /// and it prints an error as its `slopbar-mcp` would (see `failed`).
    struct SharingTarget {
        /// What `--sharing-state` answers.
        link: Value,
        /// `--sharing-state` fails with this door code instead (printed raw, as `mcp::sharing_state` does).
        link_error: Option<&'static str>,
        /// What `get_sharing_setup` answers.
        setup: Value,
        /// A setup already runs there: starting another is refused.
        busy: bool,
        /// Starting is refused with this error (an unusable remote name, say).
        refuse: Option<&'static str>,
        /// Someone can see its desktop: a setup whose page opens there is refused while nobody can,
        /// before anything else is looked at (app.rs `start_sharing_setup`), as one with
        /// `via_tunnel` never is.
        desktop: bool,
        /// Its SlopBar prints an error's code beside its words; else it is one from before that.
        codes: bool,
        /// Its SlopBar has the guided setup; else it is one from before that.
        guided: bool,
        /// It has PowerShell 7 (see `FakeSsh::pwsh`).
        pwsh: bool,
        commands: RefCell<Vec<String>>,
    }

    fn target(link: Value) -> SharingTarget {
        SharingTarget { link, link_error: None, setup: json!({ "phase": "done", "found": ["DESK-2"] }), busy: false, refuse: None, desktop: true, codes: true, guided: true, pwsh: true, commands: RefCell::default() }
    }

    impl SharingTarget {
        /// The line its slopbar-mcp prints when a tool fails with `error`: the words an agent reads,
        /// with the code beside them (`mcp::error_line`), or from an earlier SlopBar the words alone.
        fn failed(&self, error: &str) -> Value {
            match (self.codes, error) {
                (true, _) => crate::mcp::error_line(error),
                (false, "sharing_setup_busy") => json!({ "error": BUSY_BEFORE_CODES }),
                (false, _) => json!({ "error": crate::mcp::explain(error) }),
            }
        }
    }

    fn call_b64(tool: &str, arguments: Value) -> String {
        remote_command(&["--call-b64", &base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, json!({ "tool": tool, "arguments": arguments }).to_string())])
    }

    impl Remote for SharingTarget {
        fn run(&self, _host: &str, command: &str, _input: &[u8]) -> Result<Vec<u8>, String> {
            self.commands.borrow_mut().push(command.into());
            if !shell_runs(self.pwsh, command) {
                return not_found();
            }
            let (_, args) = parts(command);
            let out = match tool_asked(&args) {
                None if args == ["--sharing-state", STATE_WAIT] => match self.link_error {
                    Some(error) => json!({ "error": error }),
                    None => json!({ "ok": self.link }),
                },
                // From before the guided setup: its slopbar-mcp refuses a tool it lacks so (see
                // `mcp::run_tool`).
                Some((tool, _)) if !self.guided => json!({ "error": format!("Unknown tool: {tool}") }),
                Some((tool, _)) if tool == "get_sharing_setup" => json!({ "ok": self.setup }),
                Some((_, arguments)) if arguments["cancel"] == true => json!({ "ok": { "phase": "failed", "error": "cancelled" } }),
                Some((tool, arguments)) if tool == "setup_sharing" => match self.refuse {
                    Some(error) => self.failed(error),
                    None if !self.desktop && arguments["via_tunnel"] != true => self.failed("desktop_locked"),
                    None if self.busy => self.failed("sharing_setup_busy"),
                    None => json!({ "ok": { "phase": "rclone" } }),
                },
                _ => panic!("not asked of this computer: {args:?}"),
            };
            answered(&out)
        }
    }

    #[test]
    fn another_computers_page_opens_on_its_own_screen_by_default() {
        let here = FakeApp::new(json!({}));
        let there = target(json!({ "phase": "dropbox", "state": "Ab-9_x" }));
        let out = setup_remote_sharing(&here, &there, "box", &json!({})).unwrap();
        assert_eq!(out["phase"], "rclone", "that computer's own state: {out}");
        assert!(out["next"].as_str().unwrap().contains("its own browser"), "{out}");
        assert_eq!(*there.commands.borrow(), [call_b64("setup_sharing", json!({ "confirm": true }))], "no tunnel, no state asked for");
        assert!(matches!(&here.seen.borrow()[..], [Request::ListSharingLocations]), "nothing opens here");

        // Nobody can see that desktop: refused there in its words, said here to be about that computer.
        for codes in [true, false] {
            let here = FakeApp::new(json!({}));
            let there = SharingTarget { desktop: false, codes, ..target(json!({})) };
            let error = setup_remote_sharing(&here, &there, "box", &json!({})).unwrap_err();
            assert_eq!(error, format!("target: {}", crate::mcp::explain("desktop_locked")));
            assert!(crate::mcp::explain(&error).starts_with("On the other computer: Nothing was started: "), "{error}");
        }

        // Cancelling stops it there (and closes any tunnel here).
        let here = FakeApp::new(json!({}));
        let there = target(json!({}));
        setup_remote_sharing(&here, &there, "box", &json!({ "cancel": true })).unwrap();
        assert_eq!(there.commands.borrow()[0], call_b64("setup_sharing", json!({ "cancel": true })));
        assert!(matches!(&here.seen.borrow()[..], [Request::SharingTunnel { state: None, close: true, .. }]));
    }

    #[test]
    fn with_open_here_another_computers_sign_in_opens_here_and_its_state_never_reaches_the_agent() {
        let here = FakeApp::new(json!({}));
        let there = target(json!({ "phase": "dropbox", "state": "Ab-9_x" }));
        let out = setup_remote_sharing(&here, &there, "box", &json!({ "open_here": true })).unwrap();
        assert_eq!(out["phase"], "dropbox");
        assert!(!out.to_string().contains("Ab-9_x"), "{out}");
        assert_eq!(there.commands.borrow()[0], call_b64("setup_sharing", json!({ "confirm": true, "via_tunnel": true })), "that computer's browser is not needed");
        let seen = here.seen.borrow();
        assert!(matches!(&seen[..], [Request::ListSharingLocations, Request::SharingTunnel { host, state: Some(s), close: false }] if host == "box" && s == "Ab-9_x"), "{seen:?}");

        // Already connected there (no sign-in to wait for): its own state answers, and no tunnel opens.
        let here = FakeApp::new(json!({}));
        let there = target(json!({ "phase": "done", "state": null }));
        assert_eq!(setup_remote_sharing(&here, &there, "box", &json!({ "open_here": true, "remote": "dropbox", "reconnect": false })).unwrap()["found"], json!(["DESK-2"]));
        assert!(here.seen.borrow().is_empty());
        assert_eq!(there.commands.borrow()[0], call_b64("setup_sharing", json!({ "confirm": true, "via_tunnel": true, "remote": "dropbox" })));
    }

    /// A second setup_sharing with ssh_host (a call that gave up while that computer got rclone,
    /// made again as its answer says) joins the setup running there, whether that computer's
    /// SlopBar prints codes or only words: nothing starts twice, and nothing opens here without
    /// open_here.
    #[test]
    fn a_second_setup_joins_the_one_running_there() {
        let running = |via_tunnel: bool| json!({ "phase": "rclone", "remote": "dropbox", "via_tunnel": via_tunnel, "revision": 4 });
        let asked = |there: &SharingTarget| there.commands.borrow().iter().map(|c| asked_of(c)).collect::<Vec<_>>();
        let wait = format!("--sharing-state {STATE_WAIT}");
        for codes in [true, false] {
            let busy = |setup: Value| SharingTarget { busy: true, codes, setup, ..target(json!({ "phase": "dropbox", "state": "Ab-9_x" })) };

            // Started to open on that computer's screen: joined there, with what to do next.
            let here = FakeApp::new(json!({}));
            let there = busy(running(false));
            let out = setup_remote_sharing(&here, &there, "box", &json!({})).unwrap();
            assert_eq!((out["phase"].as_str(), out["revision"].as_u64()), (Some("rclone"), Some(4)), "{codes}: {out}");
            assert_eq!(out["next"], SETUP_THERE);
            assert_eq!(asked(&there), ["setup_sharing", "get_sharing_setup"], "nothing started twice");
            assert!(matches!(&here.seen.borrow()[..], [Request::ListSharingLocations]), "nothing opens here");

            // Joined with open_here: its sign-in opens here once it waits, however it was started,
            // and with that computer's desktop locked too (open_here's own case): a setup whose page
            // opens elsewhere is never refused for it.
            for (via_tunnel, desktop) in [(true, true), (true, false), (false, true)] {
                let here = FakeApp::new(json!({}));
                let there = SharingTarget { desktop, ..busy(running(via_tunnel)) };
                let out = setup_remote_sharing(&here, &there, "box", &json!({ "open_here": true, "confirm": true })).unwrap();
                assert_eq!((out["phase"].as_str(), out["browser"].as_str()), (Some("dropbox"), Some("this computer")), "{codes} {via_tunnel} {desktop}: {out}");
                assert!(!out.to_string().contains("Ab-9_x"), "{out}");
                assert!(here.seen.borrow().iter().any(|r| matches!(r, Request::SharingTunnel { state: Some(s), close: false, .. } if s == "Ab-9_x")));
                assert_eq!(asked(&there), ["setup_sharing", "get_sharing_setup", wait.as_str()]);
            }

            // Started with open_here, and joined by a call without it: nothing opens here (the user
            // may not want the page on this computer); its answer says how to finish it here.
            let here = FakeApp::new(json!({}));
            let there = busy(running(true));
            let out = setup_remote_sharing(&here, &there, "box", &json!({})).unwrap();
            assert_eq!((out["phase"].as_str(), out["via_tunnel"].as_bool()), (Some("rclone"), Some(true)), "{codes}: {out}");
            assert_eq!(out["next"], SETUP_ELSEWHERE);
            assert!(matches!(&here.seen.borrow()[..], [Request::ListSharingLocations]), "nothing opens here");
            assert_eq!(asked(&there), ["setup_sharing", "get_sharing_setup"]);
            // A call without open_here asks for the page on that computer's screen, which is
            // refused while nobody can see it, before the setup running there is looked at.
            let there = SharingTarget { desktop: false, ..busy(running(true)) };
            let error = setup_remote_sharing(&FakeApp::new(json!({})), &there, "box", &json!({})).unwrap_err();
            assert_eq!(error, format!("target: {}", crate::mcp::explain("desktop_locked")));

            // It ended between the refusal and the look: its own state answers, and nothing opens.
            let here = FakeApp::new(json!({}));
            let there = busy(json!({ "phase": "done", "found": ["DESK-2"] }));
            let out = setup_remote_sharing(&here, &there, "box", &json!({ "open_here": true })).unwrap();
            assert_eq!(out, json!({ "phase": "done", "found": ["DESK-2"] }));
            assert!(matches!(&here.seen.borrow()[..], [Request::ListSharingLocations]));
        }
        // A SlopBar from before the guided setup: update it there.
        let here = FakeApp::new(json!({}));
        let there = SharingTarget { guided: false, codes: false, ..target(json!({})) };
        assert_eq!(setup_remote_sharing(&here, &there, "box", &json!({})).unwrap_err(), "update_slopbar_there");
        assert_eq!(asked(&there), ["setup_sharing"]);
    }

    /// The tunnel here failing (port 53682 taken, SSH, or this computer's desktop locked) stops the
    /// setup there only when this call started it: one it joined may be someone else's, at that
    /// computer's screen or another computer's, and goes on.
    #[test]
    fn a_tunnel_that_fails_stops_only_the_setup_this_call_started() {
        let asked = |there: &SharingTarget| there.commands.borrow().iter().map(|c| asked_of(c)).collect::<Vec<_>>();
        let (wait, waiting) = (format!("--sharing-state {STATE_WAIT}"), json!({ "phase": "dropbox", "state": "Ab-9_x" }));
        for failure in ["tunnel_failed", "desktop_locked"] {
            let here = FakeApp { tunnel_error: Some(failure), ..FakeApp::new(json!({})) };
            let there = target(waiting.clone());
            assert_eq!(setup_remote_sharing(&here, &there, "box", &json!({ "open_here": true })).unwrap_err(), failure);
            assert_eq!(asked(&there), ["setup_sharing", wait.as_str(), "setup_sharing"]);
            assert_eq!(there.commands.borrow()[2], call_b64("setup_sharing", json!({ "cancel": true })), "stopped there");

            let here = FakeApp { tunnel_error: Some(failure), ..FakeApp::new(json!({})) };
            let there = SharingTarget { busy: true, setup: json!({ "phase": "dropbox", "via_tunnel": false }), ..target(waiting.clone()) };
            let error = setup_remote_sharing(&here, &there, "box", &json!({ "open_here": true })).unwrap_err();
            assert_eq!(asked(&there), ["setup_sharing", "get_sharing_setup", wait.as_str()], "{failure}: not stopped there");
            // Said so: the tunnel's own words would say the setup there was stopped.
            assert_eq!(error, if failure == "tunnel_failed" { "tunnel_failed_joined" } else { failure });
        }
    }

    /// What fails on the other computer while its setup is followed is said to be that computer's:
    /// its door's own codes (`--sharing-state` prints them raw) would otherwise be worded here as
    /// this computer's ("SlopBar is not running…").
    #[test]
    fn what_fails_on_the_other_computer_is_said_to_be_its() {
        for (there, args) in [
            (SharingTarget { link_error: Some("not_running"), ..target(json!({})) }, json!({ "open_here": true })),
            (SharingTarget { link_error: Some("not_running"), busy: true, setup: json!({ "phase": "rclone", "via_tunnel": true }), ..target(json!({})) }, json!({ "open_here": true })),
        ] {
            let error = setup_remote_sharing(&FakeApp::new(json!({})), &there, "box", &args).unwrap_err();
            assert_eq!(error, "target: not_running");
            assert!(crate::mcp::explain(&error).starts_with("On the other computer: "), "{error}");
        }
    }

    /// Every `next` setup_sharing with ssh_host gives reaches the agent whole (each string a tool
    /// returns is cut at `mcp::MAX_TEXT`), and each that says to call again keeps open_here: a call
    /// without it asks for the page on that computer's screen, refused while nobody can see it.
    #[test]
    fn what_setting_up_another_computer_says_next_reaches_the_agent_whole() {
        for next in [SETUP_SHARING_PREVIEW, SETUP_THERE, SETUP_ELSEWHERE, SETUP_HERE, SETUP_NOT_SHOWN, SETUP_STILL_GOING] {
            assert!(next.chars().count() <= crate::mcp::MAX_TEXT, "{} characters: {next}", next.chars().count());
        }
        for again in [SETUP_ELSEWHERE, SETUP_NOT_SHOWN, SETUP_STILL_GOING] {
            assert!(again.contains("call setup_sharing again with this ssh_host, open_here: true and confirm: true") || again.contains("Call setup_sharing again with this ssh_host, open_here: true and confirm: true"), "{again}");
        }
        assert_eq!(with_next(json!(["not", "an", "object"]), SETUP_THERE), json!(["not", "an", "object"]), "never indexed into");
        assert_eq!(with_next(json!({ "phase": "rclone" }), SETUP_THERE), json!({ "phase": "rclone", "next": SETUP_THERE }));
    }

    #[test]
    fn a_computer_without_powershell_7_is_set_up_through_windows_powershell() {
        let here = FakeApp::new(json!({}));
        let there = SharingTarget { pwsh: false, ..target(json!({ "phase": "dropbox", "state": "Ab-9_x" })) };
        let out = setup_remote_sharing(&here, &there, "box", &json!({ "open_here": true })).unwrap();
        assert_eq!(out["phase"], "dropbox");
        assert!(!out.to_string().contains("Ab-9_x"), "{out}");
        assert!(here.seen.borrow().iter().any(|r| matches!(r, Request::SharingTunnel { state: Some(s), .. } if s == "Ab-9_x")));
        // Each ask tries PowerShell 7 first, then the same one under Windows PowerShell.
        let commands = there.commands.borrow();
        let tried: Vec<(String, String)> = commands.iter().map(|c| (parts(c).0, asked_of(c))).collect();
        let step = |shell: &str, asked: &str| (shell.to_string(), asked.to_string());
        let wait = format!("--sharing-state {STATE_WAIT}");
        assert_eq!(tried, [step("pwsh", "setup_sharing"), step("powershell", "setup_sharing"), step("pwsh", &wait), step("powershell", &wait)]);
        assert_eq!(script_of(&commands[0]), script_of(&commands[1]));
    }

    #[test]
    fn another_computer_takes_this_ones_dropbox_remote() {
        // This computer shares through a Dropbox remote: that computer uses the same name.
        let here = FakeApp { remotes: vec!["work-dropbox"], ..FakeApp::new(json!({ "presence_remote": "work-dropbox" })) };
        let there = target(json!({}));
        setup_remote_sharing(&here, &there, "box", &json!({ "reconnect": true })).unwrap();
        assert_eq!(there.commands.borrow()[0], call_b64("setup_sharing", json!({ "confirm": true, "remote": "work-dropbox", "reconnect": true })));

        // Not a Dropbox one: that computer keeps its own default.
        let here = FakeApp { remotes: vec!["gdrive"], ..FakeApp::new(json!({ "presence_remote": "gdrive" })) };
        let there = target(json!({}));
        setup_remote_sharing(&here, &there, "box", &json!({})).unwrap();
        assert_eq!(there.commands.borrow()[0], call_b64("setup_sharing", json!({ "confirm": true })));
    }

    #[test]
    fn setting_up_sharing_waits_for_the_go_ahead() {
        for args in [json!({}), json!({ "ssh_host": "box" }), json!({ "confirm": false, "remote": "dropbox" })] {
            // Nothing is reached (no app, no SSH) before confirm: true.
            let out = local_tool("setup_sharing", &args, "unused-pipe", true).unwrap();
            assert_eq!(out["started"], false, "{args}");
            assert!(out["next"].as_str().unwrap().contains("whole Dropbox"));
        }
        // Every string a tool returns is cut at the door's text cap: the preview fits, so the agent
        // keeps its last sentence, when to confirm.
        let n = SETUP_SHARING_PREVIEW.chars().count();
        assert!(n <= crate::mcp::MAX_TEXT, "{n} characters");
        assert!(SETUP_SHARING_PREVIEW.ends_with("Confirm only if the user asked for this setup."));
    }

    #[test]
    fn a_finished_setup_elsewhere_says_whether_it_sees_this_computer() {
        let mut seen = json!({ "phase": "done", "found": ["desk-1", "LAPTOP"] });
        seen_from_there(&mut seen, "DESK-1");
        assert_eq!(seen["sees_this_computer"], true);
        assert!(seen.get("next").is_none());
        let mut unseen = json!({ "phase": "done", "found": [] });
        seen_from_there(&mut unseen, "DESK-1");
        assert_eq!(unseen["sees_this_computer"], false);
        assert!(unseen["next"].as_str().unwrap().contains("reconnect: true"));
    }

    #[test]
    fn a_computer_called_over_ssh_never_sets_up_a_third_one() {
        for tool in ["setup_sharing", "get_sharing_setup"] {
            assert_eq!(local_tool(tool, &json!({ "ssh_host": "third" }), "unused-pipe", false).unwrap_err(), "not_from_a_remote_call");
        }
    }

    fn ssh(target: &FakeApp, running: bool) -> FakeSsh<'_> {
        FakeSsh { target, running, echo: false, echo_refusal: false, pwsh: true, commands: RefCell::default(), inputs: RefCell::default() }
    }

    #[test]
    fn pairing_moves_a_new_key_only_through_ssh_input() {
        // A target with PowerShell 7, and one with only Windows PowerShell.
        for pwsh in [true, false] {
            let source = FakeApp::new(json!({ "presence_remote": "dropbox", "presence_folder": null, "account_roster": false }));
            let target = FakeApp::new(json!({ "presence_remote": null, "presence_folder": null, "account_roster": false }));
            let link = FakeSsh { pwsh, ..ssh(&target, true) };
            let result = pair(&source, &link, "box", None, "DESK-1", false).unwrap();
            let key = source.key_set().expect("the source saved a new key");
            assert!(key.starts_with(KEY_PREFIX));
            assert_eq!(target.key_set().as_deref(), Some(key.as_str()), "the target saved the same key");
            assert_eq!(source.patches(), [json!({ "account_roster": true })]);
            assert_eq!(target.patches(), [json!({ "presence_remote": "dropbox", "account_roster": true })]);
            // The key is in ssh's input, never in a command line, its script or the result: once,
            // or once for each try where the target's shell finds no pwsh (that shell exits
            // without reading it).
            let commands = link.commands.borrow();
            assert!(commands.iter().all(|c| !c.contains(&key) && !script_of(c).contains(&key)));
            let carried = link.inputs.borrow().iter().filter(|i| String::from_utf8_lossy(i).contains(&key)).count();
            assert_eq!(carried, if pwsh { 1 } else { 2 }, "sent as input only");
            let tried: Vec<(String, String)> = commands.iter().map(|c| (parts(c).0, asked_of(c))).collect();
            let asks = ["get_machine_readiness", "list_sharing_locations", "--pair-receive"];
            let expected: Vec<(String, String)> = if pwsh {
                asks.iter().map(|a| ("pwsh".to_string(), a.to_string())).collect()
            } else {
                asks.iter().flat_map(|a| [("pwsh".to_string(), a.to_string()), ("powershell".to_string(), a.to_string())]).collect()
            };
            assert_eq!(tried, expected, "PowerShell 7 first, every time; Windows PowerShell never where it runs");
            let shown = result.to_string();
            assert!(!shown.contains(&key) && !shown.contains(KEY_SHOWN), "{shown}");
            assert_eq!(result["key"], "generated");
            assert_eq!(result["target"]["roster"]["waiting"], 1, "a count, not the list of emails");
            assert_eq!(result["target"]["from"], "DESK-1");
            assert!(matches!(source.seen.borrow().last(), Some(Request::SyncRoster)), "the source reads the target's roster at once");
        }
    }

    #[test]
    fn a_roster_summary_passes_on_only_a_proxy_code_it_knows() {
        let summary = roster_summary(&json!({ "wrote": false, "write_error": "proxy_script", "read": false,
            "read_error": "anything else another computer says", "waiting": [{}, {}] }));
        assert_eq!((&summary["wrote"], &summary["write_error"]), (&json!(false), &json!("proxy_script")));
        assert_eq!((&summary["read"], &summary["read_error"]), (&json!(false), &Value::Null));
        assert_eq!(summary["waiting"], 2);
    }

    #[test]
    fn pairing_reuses_the_saved_passphrase() {
        let source = FakeApp::new(json!({ "presence_remote": "dropbox", "account_roster": true }));
        let target = FakeApp::new(json!({ "presence_remote": "dropbox", "account_roster": true }));
        let mut target = target;
        target.saved = Some("synthetic saved passphrase");
        let result = pair(&source, &ssh(&target, true), "box", Some(Passphrase("synthetic saved passphrase".into())), "A", false).unwrap();
        assert_eq!(result["key"], "reused");
        assert!(source.key_set().is_none(), "the source keeps its passphrase");
        assert!(source.patches().is_empty() && target.patches().is_empty(), "nothing else to change");
        assert_eq!(target.key_set().as_deref(), Some("synthetic saved passphrase"));
        assert_eq!(result["target"]["replaced_key"], false, "the same key again is no change");
    }

    #[test]
    fn a_target_keeps_the_key_its_other_computers_share_unless_told() {
        let source = FakeApp::new(json!({ "presence_remote": "dropbox", "account_roster": true }));
        let mut target = FakeApp::new(json!({ "presence_remote": "dropbox", "account_roster": true }));
        target.saved = Some("synthetic other passphrase");
        let mine = || Some(Passphrase("synthetic saved passphrase".into()));
        assert_eq!(pair(&source, &ssh(&target, true), "box", mine(), "A", false).unwrap_err(), format!("target: {}", crate::mcp::explain("target_has_other_key")));
        assert!(target.key_set().is_none() && target.patches().is_empty(), "nothing changed there");
        let result = pair(&source, &ssh(&target, true), "box", mine(), "A", true).unwrap();
        assert_eq!(result["target"]["replaced_key"], true);
        assert_eq!(target.key_set().as_deref(), Some("synthetic saved passphrase"));
    }

    #[test]
    fn pairing_changes_nothing_when_either_side_cannot_take_it() {
        let nowhere = FakeApp::new(json!({ "presence_remote": null, "presence_folder": null }));
        let target = FakeApp::new(json!({}));
        assert_eq!(pair(&nowhere, &ssh(&target, true), "box", None, "A", false).unwrap_err(), "no_sharing_location");
        assert_eq!(pair(&nowhere, &ssh(&target, true), "-oProxyCommand=x", None, "A", false).unwrap_err(), "bad_ssh_host");
        let source = FakeApp::new(json!({ "presence_remote": "dropbox", "account_roster": false }));
        assert_eq!(pair(&source, &ssh(&target, false), "box", None, "A", false).unwrap_err(), "target_not_running");
        assert!(source.key_set().is_none() && source.patches().is_empty(), "the target is checked before anything changes here");
        // A target without the source's rclone remote: nothing changes on either computer.
        let mut bare = FakeApp::new(json!({ "presence_remote": null, "presence_folder": null }));
        bare.remotes = vec![];
        let err = pair(&source, &ssh(&bare, true), "box", None, "A", false).unwrap_err();
        assert_eq!(err, "target: remote_missing_here: dropbox");
        assert!(bare.key_set().is_none() && bare.patches().is_empty());
        assert!(source.key_set().is_none() && source.patches().is_empty(), "checked before anything changed here");
        // The setting names the remote but rclone there does not have it (or is not installed).
        let mut unset = FakeApp::new(json!({ "presence_remote": "dropbox", "presence_folder": null }));
        unset.remotes = vec![];
        assert_eq!(pair(&source, &ssh(&unset, true), "box", None, "A", false).unwrap_err(), "target: remote_missing_here: dropbox");
        // A folder-sharing source and a target with no location at all.
        let folder = FakeApp::new(json!({ "presence_remote": null, "presence_folder": "C:/Dropbox", "account_roster": true }));
        assert_eq!(pair(&folder, &ssh(&bare, true), "box", None, "A", false).unwrap_err(), "no_sharing_location_here");
    }

    #[test]
    fn a_target_echoing_the_key_never_shows_it() {
        let source = FakeApp::new(json!({ "presence_remote": "dropbox", "account_roster": true }));
        let target = FakeApp::new(json!({ "presence_remote": "dropbox", "account_roster": true }));
        let mut link = ssh(&target, true);
        link.echo = true;
        let result = pair(&source, &link, "box", Some(Passphrase("synthetic saved passphrase".into())), "A", false).unwrap();
        assert!(!result.to_string().contains("synthetic saved passphrase"), "{result}");
        assert!(result.to_string().contains("[key]"));
        // Nor when it refuses with the key, as its words and as its code (this one reads as a code).
        let target = FakeApp::new(json!({ "presence_remote": "dropbox", "account_roster": true }));
        let link = FakeSsh { echo_refusal: true, ..ssh(&target, true) };
        let error = pair(&source, &link, "box", Some(Passphrase("synthetic saved passphrase".into())), "A", false).unwrap_err();
        assert_eq!(error, "target: [key]");
    }

    #[test]
    fn receiving_rejects_anything_but_an_offer() {
        let target = FakeApp::new(json!({ "presence_folder": "C:/Dropbox" }));
        for bad in [&b""[..], b"not json", b"{\"format\":\"slopbar-pairing\",\"v\":2,\"key\":\"synthetic key 123\",\"presence_remote\":null,\"from\":\"A\"}",
                    b"{\"format\":\"other\",\"v\":1,\"key\":\"synthetic key 123\",\"presence_remote\":null,\"from\":\"A\"}"] {
            assert!(receive(&target, &mut &bad[..], None).is_err());
        }
        assert!(target.key_set().is_none());
        // A folder-sharing source: the target keeps its own folder.
        let ok = br#"{"format":"slopbar-pairing","v":1,"key":"synthetic key 123","presence_remote":null,"from":"A"}"#;
        receive(&target, &mut &ok[..], None).unwrap();
        assert_eq!(target.patches(), [json!({ "account_roster": true })]);
        let none = FakeApp::new(json!({ "presence_remote": null, "presence_folder": null }));
        assert_eq!(receive(&none, &mut &ok[..], None).unwrap_err(), "no_sharing_location_here");
        let huge = format!(r#"{{"format":"slopbar-pairing","v":1,"key":"{}","presence_remote":null,"from":"A"}}"#, "é".repeat(1024));
        receive(&target, &mut huge.as_bytes(), None).expect("the longest passphrase fits");
        let too_big = vec![b' '; MAX_OFFER as usize + 1];
        assert_eq!(receive(&target, &mut &too_big[..], None).unwrap_err(), "pairing_unreadable");
    }

    #[test]
    fn readiness_says_what_stops_sign_ins() {
        use crate::desktop::{Desktop, State};
        let roster = json!({ "enabled": true, "location": true, "passphrase_saved": true, "unreadable": 0 });
        let at = |state, locked| Some(Desktop { session: 1, state, locked });
        assert_eq!(assess(true, at(State::Active, Some(false)), true, &roster).0, true);
        let (ready, problems, next) = assess(true, None, true, &roster);
        assert!(!ready && problems == ["no_desktop_session"] && next.contains("Nobody is signed in"));
        assert_eq!(assess(true, at(State::Active, Some(true)), true, &roster).1, ["desktop_locked"]);
        assert_eq!(assess(true, at(State::Disconnected, None), false, &roster).1, ["desktop_not_shown", "no_browser"]);
        assert_eq!(assess(false, None, true, &Value::Null).1, ["app_not_running", "no_desktop_session"]);
        let (ready, problems, _) = assess(true, at(State::Active, None), true, &json!({ "enabled": false, "unreadable": 2 }));
        assert!(ready, "sign-ins can still run without a roster");
        assert_eq!(problems, ["roster_off", "no_sharing_location", "no_roster_key", "roster_unreadable"]);
    }

    #[test]
    fn ssh_shells_are_told_by_their_kind_never_their_path() {
        let there = |_: &str| true;
        assert_eq!(shell_kind(None, there), "cmd", "OpenSSH's own default");
        assert_eq!(shell_kind(Some(""), there), "cmd");
        for pwsh in [r"C:\Program Files\PowerShell\7\pwsh.exe", "C:/Program Files/PowerShell/7/pwsh.exe", r"C:\Users\synthetic\AppData\Local\Microsoft\WindowsApps\pwsh.exe", r"C:\Program Files\PowerShell\7\pwsh.exe  "] {
            assert_eq!(shell_kind(Some(pwsh), there), "pwsh", "{pwsh:?}");
        }
        assert_eq!(shell_kind(Some(r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\PowerShell.EXE"), there), "windows_powershell");
        assert_eq!(shell_kind(Some(r"C:\Windows\System32\cmd.exe"), there), "cmd");
        for other in [r"C:\Program Files\Git\bin\bash.exe", r"C:\tools\pwsh.cmd"] {
            assert_eq!(shell_kind(Some(other), there), "other", "{other}");
        }
        // A path without an extension starts its .exe, as CreateProcess does; one rooted at \ or /
        // (the current drive, or a share) is a path too.
        let only_exe = |p: &str| p.ends_with(".exe");
        assert_eq!(shell_kind(Some(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell"), only_exe), "windows_powershell");
        for rooted in [r"\Windows\System32\WindowsPowerShell\v1.0\powershell.exe", "/Windows/System32/WindowsPowerShell/v1.0/powershell", "//server/share/powershell.exe"] {
            assert_eq!(shell_kind(Some(rooted), only_exe), "windows_powershell", "{rooted}");
        }
        // sshd starts nothing for a path to no file (or a folder), a quoted value, a relative name
        // (it never searches PATH) or an unexpanded %VAR%.
        for gone in [r"C:\Program Files\PowerShell\7\pwsh.exe", r"\\server\share\pwsh.exe", r"C:\Program Files\PowerShell\7"] {
            assert_eq!(shell_kind(Some(gone), |_| false), "missing", "{gone}");
        }
        for gone in [r#""C:\Program Files\PowerShell\7\pwsh.exe""#, "pwsh.exe", "pwsh", r"%ProgramFiles%\PowerShell\7\pwsh.exe"] {
            assert_eq!(shell_kind(Some(gone), there), "missing", "{gone}");
        }
    }

    #[test]
    fn registry_strings_end_at_their_first_nul_as_sshd_reads_them() {
        let units = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert_eq!(c_string(&units("C:\\pwsh.exe\0")), r"C:\pwsh.exe");
        assert_eq!(c_string(&units("C:\\pwsh.exe\0powershell.exe\0\0")), r"C:\pwsh.exe", "a value with a NUL inside");
        assert_eq!(c_string(&units("no terminator")), "no terminator");
        assert_eq!(c_string(&[]), "");
    }

    #[test]
    fn only_a_file_counts_as_a_shell_that_is_there() {
        let dir = crate::test_support::TempDir::new();
        let file = dir.0.join("shell.exe");
        std::fs::write(&file, b"synthetic").unwrap();
        assert!(is_file_there(file.to_str().unwrap()));
        assert!(!is_file_there(dir.0.to_str().unwrap()), "a folder starts nothing");
        assert!(!is_file_there(dir.0.join("none.exe").to_str().unwrap()));
    }

    #[test]
    fn windows_powershell_as_the_ssh_shell_never_stops_sign_ins() {
        use crate::desktop::{Desktop, State};
        let roster = json!({ "enabled": true, "location": true, "passphrase_saved": true, "unreadable": 0 });
        let at = |state, locked| Some(Desktop { session: 1, state, locked });
        for (running, desktop, browser) in [(true, at(State::Active, Some(false)), true), (false, None, true), (true, at(State::Active, Some(true)), false), (true, at(State::Disconnected, None), true)] {
            let (_, problems, next) = assess(running, desktop, browser, &roster);
            for shell in [None, Some("pwsh"), Some("cmd"), Some("other"), Some("missing")] {
                let (mut p, mut n) = (problems.clone(), next.clone());
                note_ssh_shell(shell, &mut p, &mut n);
                assert_eq!((&p, &n), (&problems, &next), "{shell:?} changes nothing");
            }
            let (mut p, mut n) = (problems.clone(), next.clone());
            note_ssh_shell(Some("windows_powershell"), &mut p, &mut n);
            assert_eq!(p.last(), Some(&SSH_SHELL_WINDOWS_POWERSHELL), "noted last");
            assert_eq!(p[..p.len() - 1], problems[..], "after what decides sign-ins");
            assert!(n.starts_with(&next) && n.ends_with(SSH_SHELL_ADVICE), "{n}");
        }
        // The longest next the app gives (the rclone advice, for the longest remote name sharing
        // takes) with both notes still fits the door's text cap, so neither advice is cut.
        let name = "r".repeat(64);
        assert!(crate::rclone::valid_remote(&name) && !crate::rclone::valid_remote(&"r".repeat(65)));
        let longest = format!("{} {SSH_SHELL_ADVICE} {NO_PWSH_ADVICE}", rclone_missing_next(&name));
        assert!(longest.chars().count() <= crate::mcp::MAX_TEXT, "{} characters", longest.chars().count());
    }

    #[test]
    fn readiness_without_the_app_names_this_computers_ssh_shell() {
        let readiness = readiness_without_app("not_running");
        let shell = readiness.get("ssh_shell").expect("always there; null with no OpenSSH server");
        assert_eq!(shell, &json!(ssh_shell()));
        let noted = readiness["problems"].as_array().unwrap().iter().any(|p| p == SSH_SHELL_WINDOWS_POWERSHELL);
        assert_eq!(noted, *shell == "windows_powershell");
    }

    #[cfg(windows)]
    #[test]
    fn registry_strings_read_as_sshd_reads_them() {
        let build = registry::hklm_string(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "CurrentBuildNumber").unwrap().unwrap();
        assert!(!build.is_empty() && build.chars().all(|c| c.is_ascii_digit()), "{build:?}");
        assert_eq!(registry::hklm_string(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", "SyntheticMissingValue").unwrap(), None);
        assert_eq!(registry::hklm_string(r"SOFTWARE\SyntheticMissingKey", "DefaultShell").unwrap(), None);
        // REG_EXPAND_SZ (as sshd's own ImagePath is) reads raw, %VAR% and all, as sshd reads its shell.
        let event_log = registry::hklm_string(r"SYSTEM\CurrentControlSet\Services\EventLog", "ImagePath").unwrap().unwrap();
        assert!(event_log.contains('%') && !event_log.contains('\0'), "{event_log:?}");
    }

    #[test]
    fn the_apps_readiness_notes_the_ssh_shell_off_its_async_threads() {
        let app = include_str!("app.rs").replace("\r\n", "\n");
        let readiness = &app[app.find("async fn readiness(app: &AppHandle)").unwrap()..];
        let readiness = &readiness[..readiness.find("\n}\n").unwrap()];
        for wired in ["spawn_blocking(pairing::ssh_shell)", "pairing::note_ssh_shell(ssh_shell, &mut problems, &mut next)", "\"ssh_shell\": ssh_shell", "pairing::rclone_missing_next(&name)"] {
            assert!(readiness.contains(wired), "{wired}");
        }
    }

    #[test]
    fn readiness_over_ssh_says_which_powershell_ran_it() {
        let readiness = json!({ "ok": { "ready_for_sign_in": true, "problems": ["roster_off"], "next": "Ready: someone can complete sign-ins at this computer's screen." } });
        let printed_readiness = format!("{STARTED}\r\n{}\r\n", ascii_json(&readiness));
        // pwsh ran it: said so, nothing else changes.
        let there = scripted(vec![printed(&printed_readiness)]);
        let out = remote_readiness(&there, "box", &json!({})).unwrap();
        assert_eq!(out["ssh_powershell"], "pwsh");
        assert_eq!((&out["problems"], &out["next"]), (&readiness["ok"]["problems"], &readiness["ok"]["next"]));
        // pwsh never started it there, so Windows PowerShell ran it: a problem of its own, which never
        // stops sign-ins, with the same ask (waiting passed on) on the second try.
        let there = scripted(vec![printed(""), printed(&printed_readiness)]);
        let out = remote_readiness(&there, "box", &json!({ "wait_seconds": 30 })).unwrap();
        assert_eq!(out["ssh_powershell"], "windows_powershell");
        assert_eq!(out["problems"], json!(["roster_off", NO_PWSH_OVER_SSH]), "noted last");
        assert_eq!(out["ready_for_sign_in"], true);
        let next = out["next"].as_str().unwrap();
        assert!(next.starts_with("Ready:") && next.ends_with(NO_PWSH_ADVICE), "{next}");
        let asked: Vec<_> = there.commands.borrow().iter().map(|c| { let (shell, args) = parts(c); (shell, tool_asked(&args)) }).collect();
        let readiness_ask = Some(("get_machine_readiness".to_string(), json!({ "wait_seconds": 30 })));
        assert_eq!(asked, [("pwsh".to_string(), readiness_ask.clone()), ("powershell".to_string(), readiness_ask)]);
        // An answer that is not an object passes through untouched; an error stays an error.
        let odd = format!("{STARTED}\r\n{{\"ok\":[1]}}\r\n");
        assert_eq!(remote_readiness(&scripted(vec![printed(&odd)]), "box", &json!({})).unwrap(), json!([1]));
        let failed = format!("{STARTED}\r\n{{\"error\":\"door_unavailable\"}}\r\n");
        assert_eq!(remote_readiness(&scripted(vec![printed(&failed)]), "box", &json!({})).unwrap_err(), "door_unavailable");
    }

    #[test]
    fn remote_tools_are_limited_and_never_chain() {
        assert_eq!(local_tool("run_on_machine", &json!({ "ssh_host": "box", "tool": "set_roster_passphrase" }), "x", true).unwrap_err().split(' ').next(), Some("run_on_machine"));
        assert_eq!(local_tool("pair_machine", &json!({ "ssh_host": "box" }), "x", false).unwrap_err(), "not_from_a_remote_call");
        assert!(local_tool("pair_machine", &json!({}), "x", true).unwrap_err().contains("ssh_host is required"));
        assert!(check_remote("update_settings", &json!({ "patch": { "presence_remote": "dropbox", "account_roster": true } })).is_ok());
        assert!(check_remote("update_settings", &json!({ "patch": { "phone": { "url": "https://ntfy.sh/x" } } })).unwrap_err().contains("not phone"));
        assert!(check_remote("update_settings", &json!({})).is_err());
        assert!(check_remote("sign_in_all", &json!({ "confirm": true })).is_ok());
        for t in REMOTE_TOOLS {
            assert!(!t.contains("passphrase") && !t.contains("backup") && !t.contains("remove"), "{t}");
        }
    }

    #[test]
    fn paired_hosts_are_remembered_by_machine_id_and_never_options() {
        let dir = crate::test_support::TempDir::new();
        const ID: &str = "22222222-2222-4222-8222-222222222222";
        assert_eq!(host_for(&dir.0, ID), None);
        remember_host(&dir.0, ID, "box");
        assert_eq!(host_for(&dir.0, ID).as_deref(), Some("box"));
        remember_host(&dir.0, ID, "user@box.tail1234.ts.net");
        assert_eq!(host_for(&dir.0, ID).as_deref(), Some("user@box.tail1234.ts.net"), "the last host it answered on");
        remember_host(&dir.0, ID, "-oProxyCommand=x");
        remember_host(&dir.0, "../../evil", "box");
        assert_eq!(host_for(&dir.0, ID).as_deref(), Some("user@box.tail1234.ts.net"));
        assert_eq!(host_for(&dir.0, "../../evil"), None);
        std::fs::write(dir.0.join(HOSTS_FILE), format!("{{\"{ID}\": \"-oProxyCommand=x\"}}")).unwrap();
        assert_eq!(host_for(&dir.0, ID), None, "a changed file never hands ssh an option");
    }
}
