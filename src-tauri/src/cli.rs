//! Where a provider's CLI is on Windows, and how to start it, for the sign-ins SlopBar runs
//! (`accounts::has_cli`, `login::command`).
//!
//! `where.exe` searched SlopBar's own PATH, the one Windows gave it when it started. A CLI installed
//! since stayed invisible until the next Windows sign-in, and a computer whose only Claude Code was
//! the Claude desktop app's own copy had none at all. The CLI is now looked for in this order, the
//! first hit winning:
//! 1. on PATH as a terminal opened now has it (`fresh_path`, Windows' own reading of the registry),
//!    then in the folders only SlopBar's own PATH has;
//! 2. in its installers' usual folders, which need not be on PATH: `%USERPROFILE%\.local\bin`
//!    (Anthropic's installer, which leaves PATH to the user), `%LOCALAPPDATA%\Programs\OpenAI\Codex\bin`
//!    (OpenAI's), `%APPDATA%\npm` (npm's default global folder) and
//!    `%LOCALAPPDATA%\Microsoft\WinGet\Links` (winget);
//! 3. Claude Code only: the copy the Claude desktop app downloads for its own sessions,
//!    `%APPDATA%\Claude\claude-code\<version>\<id>\claude.exe` (or the same in the packaged app's
//!    redirected folder), newest version first, and only one the app marked verified (`.verified`)
//!    whose size is the one it recorded (`.payload`).
//!
//! A folder counts the way cmd.exe reads it for the bare name: the first PATHEXT extension with a
//! file there, which must be one cmd runs itself (.exe, .cmd, .bat, .com). The sign-in keeps running
//! the bare name (`login::action`), so cmd never parses a path. Its process gets SlopBar's own PATH
//! with what Windows has added since, and the found folder first when cmd would otherwise run
//! another file there (`launch`), all within 8,191 characters: past that, cmd.exe searches no PATH
//! at all. The Claude desktop app's own executables are never taken (`%LOCALAPPDATA%\AnthropicClaude`,
//! any Electron or Squirrel app folder, the WindowsApps execution aliases): started with a sign-in's
//! arguments they would open the desktop app.
//!
//! Nothing is started to check a candidate (as with where.exe): finding one costs some file checks
//! and one read of the environment Windows would give a new process.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::accounts::Provider;

/// Where a CLI was found (see the module comment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Path,
    Installer,
    DesktopApp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub exe: PathBuf,
    pub kind: Kind,
}

/// How to start the provider's CLI by its bare name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    pub found: Option<Found>,
    /// The process's PATH; None leaves it as it is.
    pub path: Option<OsString>,
    /// For the Claude desktop app's copy, what the app sets when it runs it: no self-update of a
    /// copy the app manages, and Windows' own certificate store (a network that inspects TLS
    /// re-signs with a root only that store has) unless NODE_EXTRA_CA_CERTS names one.
    pub env: Vec<(&'static str, &'static str)>,
}

/// What a search reads: gathered once by `Places::current`, or built by a test.
#[derive(Debug, Clone, Default)]
pub struct Places {
    /// PATH's folders in search order: `fresh_path`'s, then those only SlopBar's own PATH has.
    pub path: Vec<PathBuf>,
    /// PATH's folders for a process SlopBar starts: its own, then those only `fresh_path` has.
    pub child: Vec<PathBuf>,
    /// PATHEXT's extensions, lower case, each with its dot.
    pub pathext: Vec<String>,
    /// %USERPROFILE%, %APPDATA% and %LOCALAPPDATA%.
    pub home: Option<PathBuf>,
    pub roaming: Option<PathBuf>,
    pub local: Option<PathBuf>,
}

/// cmd.exe's own PATHEXT when the variable is missing.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";
/// Extensions cmd.exe runs itself; any other goes to a file association instead.
const NATIVE: [&str; 4] = [".exe", ".cmd", ".bat", ".com"];
/// The longest PATH cmd.exe searches, in UTF-16 units: with a longer one it finds nothing on it.
const CMD_PATH_LIMIT: usize = 8191;
/// What separates PATH's folders.
const SEPARATOR: &str = if cfg!(windows) { ";" } else { ":" };
/// The packaged Claude app's family name: its name and the hash Windows derives from the
/// publisher's signing identity, which no other publisher's package can have. A packaged app may
/// write its own redirected folder, so a `Claude_…` package of anyone else's is never a place
/// SlopBar runs a program from.
const CLAUDE_PACKAGE: &str = "Claude_pzs8sxrjxfjjc";

/// The CLI's bare name, as `login::action` runs it.
pub fn name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

/// The CLI's name and the PowerShell line that installs it on Windows, for the message shown when
/// none is found (`cliMissing`; `CLI_INSTALL` in i18n.js holds the same).
pub fn install_hint(provider: Provider) -> (&'static str, &'static str) {
    match provider {
        Provider::Claude => ("Claude Code", "irm https://claude.ai/install.ps1 | iex"),
        Provider::Codex => ("Codex CLI", r#"powershell -ExecutionPolicy ByPass -c "irm https://chatgpt.com/codex/install.ps1 | iex""#),
    }
}

impl Places {
    pub fn current() -> Self {
        let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
        let (own, fresh) = (std::env::var_os("PATH"), fresh_path());
        Places {
            path: folders(&[fresh.as_deref(), own.as_deref()]),
            child: folders(&[own.as_deref(), fresh.as_deref()]),
            pathext: pathext(std::env::var_os("PATHEXT").as_deref()),
            home: var("USERPROFILE").or_else(dirs::home_dir),
            roaming: var("APPDATA"),
            local: var("LOCALAPPDATA"),
        }
    }
}

/// The provider's CLI on Windows, as things are now.
pub fn find(provider: Provider) -> Option<Found> {
    find_in(provider, &Places::current())
}

/// How to start the provider's CLI by its bare name, as things are now.
pub fn launch(provider: Provider) -> Launch {
    launch_in(provider, &Places::current(), std::env::var_os("NODE_EXTRA_CA_CERTS").is_some_and(|v| !v.is_empty()))
}

/// cmd.exe in Windows' own folder, by its full path. With PATH set for a child process, Rust looks
/// for a bare program name on that PATH first, so `cmd.exe` alone could run a file of that name
/// from the CLI's folder, or from any folder ahead of System32.
pub fn cmd_exe() -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let mut buf = [0u16; 512];
        let n = unsafe { windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n > 0 && n < buf.len() {
            return PathBuf::from(OsString::from_wide(&buf[..n])).join("cmd.exe");
        }
        PathBuf::from(r"C:\Windows\System32\cmd.exe")
    }
    #[cfg(not(windows))]
    PathBuf::from("cmd.exe")
}

pub fn find_in(provider: Provider, places: &Places) -> Option<Found> {
    let name = name(provider);
    let installers = [
        places.home.as_ref().map(|h| h.join(".local").join("bin")),
        places.local.as_ref().map(|l| l.join("Programs").join("OpenAI").join("Codex").join("bin")),
        places.roaming.as_ref().map(|r| r.join("npm")),
        places.local.as_ref().map(|l| l.join("Microsoft").join("WinGet").join("Links")),
    ];
    let mut seen = HashSet::new();
    let dirs = places.path.iter().cloned().map(|d| (d, Kind::Path))
        .chain(installers.into_iter().flatten().map(|d| (d, Kind::Installer)));
    for (dir, kind) in dirs {
        // One look at the folder first: an offline network folder then costs one wait, not one
        // per extension.
        if !seen.insert(key(&dir)) || !carriable(&dir) || !dir.is_dir() {
            continue;
        }
        match in_dir(&dir, name, &places.pathext) {
            Some(exe) if !app_folder(&dir, places.local.as_deref()) => return Some(Found { exe, kind }),
            _ => {}
        }
    }
    match provider {
        Provider::Claude => desktop_copies(places).into_iter().next().map(|exe| Found { exe, kind: Kind::DesktopApp }),
        // The Codex app's own codex.exe is a preview build the app replaces as it updates; a
        // sign-in can hold it open for 15 minutes.
        Provider::Codex => None,
    }
}

fn launch_in(provider: Provider, places: &Places, extra_ca: bool) -> Launch {
    let Some(found) = find_in(provider, places) else { return Launch::default() };
    // PATH as SlopBar's own with what Windows has added since; the found folder goes first only
    // when cmd would otherwise run another file for the bare name (a stale npm shim, say), so the
    // CLI's own lookups keep the order they had.
    let plain = fit(None, &places.child);
    let runs = plain.iter().filter(|d| d.is_absolute()).find_map(|d| first_match(d, name(provider), &places.pathext));
    let path = if runs.as_deref() == Some(found.exe.as_path()) { plain } else { fit(found.exe.parent(), &places.child) };
    let env = match found.kind {
        Kind::DesktopApp if extra_ca => vec![("DISABLE_AUTOUPDATER", "1")],
        Kind::DesktopApp => vec![("DISABLE_AUTOUPDATER", "1"), ("NODE_USE_SYSTEM_CA", "1")],
        _ => vec![],
    };
    Launch { found: Some(found), path: Some(join(&path)), env }
}

/// What cmd.exe runs in `dir` for the bare `name`: the first PATHEXT extension with a regular file
/// there.
fn first_match(dir: &Path, name: &str, pathext: &[String]) -> Option<PathBuf> {
    pathext.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|f| f.is_file())
}

/// `first_match`, when it is a file cmd runs itself; None when cmd would hand it to a file
/// association instead (a `.ps1` listed first in PATHEXT, say).
fn in_dir(dir: &Path, name: &str, pathext: &[String]) -> Option<PathBuf> {
    first_match(dir, name, pathext).filter(|f| {
        let ext = f.extension().map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()));
        ext.is_some_and(|e| NATIVE.contains(&e.as_str()))
    })
}

/// Whether PATH can carry `dir` to another process and mean the same folder there: absolute, and
/// no separator or quote in its name.
fn carriable(dir: &Path) -> bool {
    let text = dir.to_string_lossy();
    dir.is_absolute() && !text.contains(SEPARATOR) && !text.contains('"')
}

/// Whether `dir` is a desktop app's own folder, whose `claude.exe` is the app (started with a
/// sign-in's arguments, it would open the app): `%LOCALAPPDATA%\AnthropicClaude`, any Electron
/// app's folder (`resources\app.asar`), a Squirrel install's root (`Update.exe`), or the WindowsApps
/// folders of packaged apps and their execution aliases.
fn app_folder(dir: &Path, local: Option<&Path>) -> bool {
    dir.components().any(|c| c.as_os_str().eq_ignore_ascii_case("WindowsApps"))
        || local.is_some_and(|l| inside(dir, &l.join("AnthropicClaude")))
        || dir.join("resources").join("app.asar").exists()
        || dir.join("Update.exe").exists()
}

/// One spelling per folder: case, separators and a trailing separator ignored.
fn key(dir: &Path) -> String {
    dir.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

fn inside(dir: &Path, folder: &Path) -> bool {
    let (dir, folder) = (key(dir), key(folder));
    dir == folder || dir.starts_with(&format!("{folder}\\"))
}

/// The folders of the PATH values `lists`, in order, each once; empty entries dropped.
fn folders(lists: &[Option<&OsStr>]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    lists.iter().flatten().flat_map(std::env::split_paths)
        .filter(|d| !d.as_os_str().is_empty() && !d.to_string_lossy().contains('"') && seen.insert(key(d)))
        .collect()
}

/// PATHEXT's extensions, lower case, each with its dot (cmd's own list when it is missing).
fn pathext(raw: Option<&OsStr>) -> Vec<String> {
    let raw = raw.map(|v| v.to_string_lossy().into_owned()).filter(|v| !v.trim().is_empty());
    let list = raw.as_deref().unwrap_or(DEFAULT_PATHEXT);
    list.split(';').map(|e| e.trim().to_ascii_lowercase()).filter(|e| e.len() > 1 && e.starts_with('.')).collect()
}

/// One PATH entry as written: quoted when its name holds the separator.
fn entry(dir: &Path) -> OsString {
    if dir.to_string_lossy().contains(SEPARATOR) {
        let mut quoted = OsString::from("\"");
        quoted.push(dir.as_os_str());
        quoted.push("\"");
        quoted
    } else {
        dir.as_os_str().to_owned()
    }
}

fn wide_len(text: &OsStr) -> usize {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        text.encode_wide().count()
    }
    #[cfg(not(windows))]
    text.len()
}

/// `first`, then `dirs` in order, each once, keeping whole entries only while PATH stays within
/// what cmd.exe searches (`CMD_PATH_LIMIT`).
fn fit(first: Option<&Path>, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let (mut out, mut len) = (Vec::new(), 0);
    for dir in first.into_iter().chain(dirs.iter().map(PathBuf::as_path)) {
        let more = wide_len(&entry(dir)) + usize::from(!out.is_empty());
        if len + more > CMD_PATH_LIMIT || !seen.insert(key(dir)) {
            continue;
        }
        len += more;
        out.push(dir.to_owned());
    }
    out
}

fn join(dirs: &[PathBuf]) -> OsString {
    let mut path = OsString::new();
    for (i, dir) in dirs.iter().enumerate() {
        if i > 0 {
            path.push(SEPARATOR);
        }
        path.push(entry(dir));
    }
    path
}

/// The Claude desktop app's own copies of Claude Code, best first: newest version, then newest
/// file. Only copies the app finished and checked: `.verified` beside them, and the size `.payload`
/// recorded (`{"sha256", "size"}`).
fn desktop_copies(places: &Places) -> Vec<PathBuf> {
    let roots = [
        places.roaming.as_ref().map(|r| r.join("Claude")),
        places.local.as_ref().map(|l| l.join("Packages").join(CLAUDE_PACKAGE).join("LocalCache").join("Roaming").join("Claude")),
    ];
    let mut found = vec![];
    for data in roots.into_iter().flatten() {
        let Ok(versions) = std::fs::read_dir(data.join("claude-code")) else { continue };
        for version in versions.flatten() {
            let number = version_number(&version.file_name().to_string_lossy());
            let Ok(ids) = std::fs::read_dir(version.path()) else { continue };
            for id in ids.flatten() {
                let dir = id.path();
                let exe = dir.join("claude.exe");
                let Ok(meta) = std::fs::metadata(&exe) else { continue };
                if !meta.is_file() || !dir.join(".verified").is_file() || payload_size(&dir).is_some_and(|s| s != meta.len()) {
                    continue;
                }
                // What cmd would run there must be this very file.
                if !carriable(&dir) || app_folder(&dir, places.local.as_deref()) || in_dir(&dir, "claude", &places.pathext).as_deref() != Some(exe.as_path()) {
                    continue;
                }
                found.push((number.clone(), meta.modified().ok(), exe));
            }
        }
    }
    found.sort_by(|a, b| (&b.0, b.1).cmp(&(&a.0, a.1)));
    found.into_iter().map(|(_, _, exe)| exe).collect()
}

/// A version folder's name as numbers to compare ("2.1.325" is [2, 1, 325]); a name that starts
/// with no number sorts below every version.
fn version_number(name: &str) -> Vec<u64> {
    let mut parts = vec![];
    for part in name.split('.') {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        let Ok(n) = digits.parse() else { break };
        parts.push(n);
        if digits.len() != part.len() {
            break;
        }
    }
    parts
}

/// The size the desktop app recorded for a download, from its small `.payload` file.
fn payload_size(dir: &Path) -> Option<u64> {
    let file = dir.join(".payload");
    if std::fs::metadata(&file).ok()?.len() > 4096 {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&std::fs::read(file).ok()?).ok()?["size"].as_u64()
}

/// PATH as Windows gives it to a process started now: the machine's and the user's PATH from the
/// registry, expanded the way a new sign-in to Windows expands them (`CreateEnvironmentBlock`
/// without inheriting this process's environment).
#[cfg(windows)]
fn fresh_path() -> Option<OsString> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) == 0 {
            return None;
        }
        let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
        let made = CreateEnvironmentBlock(&mut block, token, 0) != 0;
        CloseHandle(token);
        if !made || block.is_null() {
            return None;
        }
        let path = variable(block as *const u16, "Path");
        DestroyEnvironmentBlock(block);
        path
    }
}

#[cfg(not(windows))]
fn fresh_path() -> Option<OsString> {
    None
}

/// `name`'s value in an environment block (`NAME=value\0…\0\0`, UTF-16), compared without case as
/// Windows does. The entries for each drive's current folder (`=C:=C:\…`) are never a match.
///
/// # Safety
/// `block` points to a complete environment block.
#[cfg(windows)]
unsafe fn variable(block: *const u16, name: &str) -> Option<OsString> {
    use std::os::windows::ffi::OsStringExt;
    let mut at = block;
    loop {
        let mut len = 0;
        while *at.add(len) != 0 {
            len += 1;
        }
        if len == 0 {
            return None;
        }
        let entry = std::slice::from_raw_parts(at, len);
        if let Some(eq) = entry.iter().skip(1).position(|&c| c == u16::from(b'=')).map(|i| i + 1) {
            if String::from_utf16_lossy(&entry[..eq]).eq_ignore_ascii_case(name) {
                return Some(OsString::from_wide(&entry[eq + 1..]));
            }
        }
        at = at.add(len + 1);
    }
}

#[cfg(test)]
mod tests;
