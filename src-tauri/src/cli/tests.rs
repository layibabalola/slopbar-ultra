use super::*;
use crate::test_support::TempDir;

fn file(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"stand-in").unwrap();
}

/// A copy of Claude Code as the Claude desktop app keeps it, with the markers it writes.
fn desktop_copy(data: &Path, version: &str, id: &str, verified: bool, recorded_size: Option<u64>) -> PathBuf {
    let dir = data.join("claude-code").join(version).join(id);
    let exe = dir.join("claude.exe");
    file(&exe);
    if verified {
        std::fs::write(dir.join(".verified"), "0".repeat(64)).unwrap();
    }
    let size = recorded_size.unwrap_or_else(|| std::fs::metadata(&exe).unwrap().len());
    std::fs::write(dir.join(".payload"), format!("{{\"sha256\": \"{}\", \"size\": {size}}}", "a".repeat(64))).unwrap();
    exe
}

/// What a search reads, with `path` both as the search order and as a child's PATH.
fn places(root: &Path, path: &[PathBuf]) -> Places {
    Places {
        path: path.to_vec(),
        child: path.to_vec(),
        pathext: pathext(None),
        home: Some(root.join("home")),
        roaming: Some(root.join("roaming")),
        local: Some(root.join("local")),
    }
}

fn found(exe: &Path, kind: Kind) -> Option<Found> {
    Some(Found { exe: exe.to_owned(), kind })
}

#[cfg(windows)]
fn system() -> PathBuf {
    cmd_exe().parent().unwrap().to_owned()
}

#[test]
fn path_comes_first_in_its_order_with_cmds_extension_order_in_each_folder() {
    let t = TempDir::new();
    let (a, b) = (t.0.join("a"), t.0.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    file(&b.join("claude.cmd"));
    file(&t.0.join("home").join(".local").join("bin").join("claude.exe"));
    let p = places(&t.0, &[a.clone(), b.clone()]);
    assert_eq!(find_in(Provider::Claude, &p), found(&b.join("claude.cmd"), Kind::Path));
    // In one folder, PATHEXT decides, not the order files were made in.
    file(&b.join("claude.exe"));
    assert_eq!(find_in(Provider::Claude, &p), found(&b.join("claude.exe"), Kind::Path));
    file(&a.join("claude.bat"));
    assert_eq!(find_in(Provider::Claude, &p), found(&a.join("claude.bat"), Kind::Path));
    // npm also writes an extension-less shell script and a .ps1: neither is what cmd runs.
    let npm = t.0.join("npm");
    file(&npm.join("codex"));
    file(&npm.join("codex.ps1"));
    assert_eq!(find_in(Provider::Codex, &places(&t.0, &[npm.clone()])), None);
    file(&npm.join("codex.cmd"));
    assert_eq!(find_in(Provider::Codex, &places(&t.0, &[npm.clone()])), found(&npm.join("codex.cmd"), Kind::Path));
    // The self-updater's leftovers never stand for the CLI.
    let fresh = TempDir::new();
    let local_bin = fresh.0.join("home").join(".local").join("bin");
    file(&local_bin.join("claude.exe.old.1794872252389.173660"));
    assert_eq!(find_in(Provider::Claude, &places(&fresh.0, &[local_bin])), None);
}

#[test]
fn a_folder_where_cmd_would_open_a_script_instead_is_skipped() {
    let t = TempDir::new();
    let (a, b) = (t.0.join("a"), t.0.join("b"));
    file(&a.join("claude.ps1"));
    file(&a.join("claude.cmd"));
    file(&b.join("claude.exe"));
    let mut p = places(&t.0, &[a.clone(), b.clone()]);
    p.pathext = pathext(Some(OsStr::new(".PS1;.COM;.EXE;.BAT;.CMD")));
    assert_eq!(find_in(Provider::Claude, &p), found(&b.join("claude.exe"), Kind::Path), "cmd would hand a's claude.ps1 to its file association");
    p.pathext = pathext(Some(OsStr::new(".COM;.EXE;.BAT;.CMD;.PS1")));
    assert_eq!(find_in(Provider::Claude, &p), found(&a.join("claude.cmd"), Kind::Path));
}

/// A terminal opened now has PATH as Windows reads it from the registry, which may differ from the
/// stale one SlopBar started with: a CLI installed since is found there first, even where SlopBar's
/// own PATH still lists a folder whose shim lost its Node.js.
#[test]
fn path_as_a_terminal_has_it_now_is_searched_first() {
    let t = TempDir::new();
    let (stale, installed) = (t.0.join("npm"), t.0.join("home").join(".local").join("bin"));
    file(&stale.join("claude.cmd"));
    file(&installed.join("claude.exe"));
    let p = Places { path: vec![installed.clone(), stale.clone()], child: vec![stale.clone(), installed.clone()], ..places(&t.0, &[]) };
    assert_eq!(find_in(Provider::Claude, &p), found(&installed.join("claude.exe"), Kind::Path));
    // The child keeps SlopBar's own order, so cmd would run the stale shim: the found folder leads.
    let launch = launch_in(Provider::Claude, &p, false);
    assert_eq!(launch.path, Some(join(&[installed.clone(), stale.clone()])));
    assert!(launch.env.is_empty(), "only the desktop app's copy runs with the app's variables");
    let own = OsString::from(r"C:\A;C:\B\;c:\a;;C:\B");
    let fresh = OsString::from(r"c:\b;C:\Users\me\.local\bin;C:\A\");
    let order = |lists: &[Option<&OsStr>]| folders(lists).iter().map(|d| d.to_string_lossy().into_owned()).collect::<Vec<_>>().join("|");
    if cfg!(windows) {
        assert_eq!(order(&[Some(&fresh), Some(&own)]), r"c:\b|C:\Users\me\.local\bin|C:\A\", "once each, case and trailing separator aside");
        assert_eq!(order(&[Some(&own), Some(&fresh)]), r"C:\A|C:\B\|C:\Users\me\.local\bin");
        assert_eq!(order(&[None, Some(&own)]), r"C:\A|C:\B\");
    }
}

/// Where cmd would run the found file anyway, PATH keeps its order: no folder moves ahead of
/// System32 or Node.js for what the CLI itself runs.
#[test]
fn the_found_folder_leads_only_when_cmd_would_run_another_file() {
    let t = TempDir::new();
    let (first, cli) = (t.0.join("first"), t.0.join("cli"));
    std::fs::create_dir_all(&first).unwrap();
    file(&cli.join("claude.exe"));
    let p = places(&t.0, &[first.clone(), cli.clone()]);
    assert_eq!(launch_in(Provider::Claude, &p, false).path, Some(join(&[first.clone(), cli.clone()])));
    // The desktop app's own claude.exe ahead of it on PATH is what cmd would run: the CLI leads.
    let app = t.0.join("local").join("AnthropicClaude");
    file(&app.join("claude.exe"));
    let p = places(&t.0, &[app.clone(), cli.clone()]);
    assert_eq!(find_in(Provider::Claude, &p), found(&cli.join("claude.exe"), Kind::Path));
    assert_eq!(launch_in(Provider::Claude, &p, false).path, Some(join(&[cli.clone(), app.clone()])));
    // Nothing found: nothing to start, and PATH is left alone.
    assert_eq!(launch_in(Provider::Codex, &p, false), Launch::default());
}

#[test]
fn the_installers_folders_count_when_path_never_got_them() {
    let t = TempDir::new();
    let p = places(&t.0, &[]);
    assert_eq!(find_in(Provider::Codex, &p), None);
    let winget = t.0.join("local").join("Microsoft").join("WinGet").join("Links").join("codex.exe");
    file(&winget);
    assert_eq!(find_in(Provider::Codex, &p), found(&winget, Kind::Installer));
    let npm = t.0.join("roaming").join("npm").join("codex.cmd");
    file(&npm);
    assert_eq!(find_in(Provider::Codex, &p), found(&npm, Kind::Installer));
    // OpenAI's own installer's folder, which it puts on the user PATH.
    let openai = t.0.join("local").join("Programs").join("OpenAI").join("Codex").join("bin").join("codex.exe");
    file(&openai);
    assert_eq!(find_in(Provider::Codex, &p), found(&openai, Kind::Installer));
    let native = t.0.join("home").join(".local").join("bin").join("claude.exe");
    file(&native);
    assert_eq!(find_in(Provider::Claude, &p), found(&native, Kind::Installer));
    assert_eq!(launch_in(Provider::Claude, &p, false).path, Some(join(&[native.parent().unwrap().to_owned()])));
    // On PATH as well: found as PATH has it, once.
    let p = places(&t.0, &[native.parent().unwrap().to_owned()]);
    assert_eq!(find_in(Provider::Claude, &p), found(&native, Kind::Path));
}

#[test]
fn claude_code_from_the_desktop_app_is_the_newest_copy_it_finished() {
    let t = TempDir::new();
    let data = t.0.join("roaming").join("Claude");
    let p = places(&t.0, &[]);
    assert_eq!(find_in(Provider::Claude, &p), None, "no copy yet");
    let old = desktop_copy(&data, "2.1.8", "a33aa5518497", true, None);
    assert_eq!(find_in(Provider::Claude, &p), found(&old, Kind::DesktopApp));
    // 2.1.9 is newer than 2.1.8, and 2.1.10 newer than 2.1.9: numbers, not text.
    let newer = desktop_copy(&data, "2.1.9", "806e0a45e6f5", true, None);
    assert_eq!(find_in(Provider::Claude, &p), found(&newer, Kind::DesktopApp));
    // Never one the app has not checked, or whose size is not the one it recorded (a download
    // still being written, or one cut short).
    desktop_copy(&data, "2.1.10", "000000000001", false, None);
    desktop_copy(&data, "2.1.10", "000000000002", true, Some(1));
    assert_eq!(find_in(Provider::Claude, &p), found(&newer, Kind::DesktopApp));
    let newest = desktop_copy(&data, "2.1.10", "000000000003", true, None);
    assert_eq!(find_in(Provider::Claude, &p), found(&newest, Kind::DesktopApp));
    // Run as the app runs it: no self-update, and Windows' certificate store unless the user named
    // certificates of their own.
    let launch = launch_in(Provider::Claude, &p, false);
    assert_eq!(launch.path, Some(join(&[newest.parent().unwrap().to_owned()])));
    assert_eq!(launch.env, vec![("DISABLE_AUTOUPDATER", "1"), ("NODE_USE_SYSTEM_CA", "1")]);
    assert_eq!(launch_in(Provider::Claude, &p, true).env, vec![("DISABLE_AUTOUPDATER", "1")]);
    // Anything installed, on PATH or in an installer's folder, comes first.
    let npm = t.0.join("roaming").join("npm").join("claude.cmd");
    file(&npm);
    assert_eq!(find_in(Provider::Claude, &p), found(&npm, Kind::Installer));
    // The desktop app's copy is Claude Code's; Codex has none.
    assert_eq!(find_in(Provider::Codex, &p), None);
}

#[test]
fn only_anthropics_own_package_is_a_place_to_run_claude_code_from() {
    let t = TempDir::new();
    let packages = t.0.join("local").join("Packages");
    let redirected = |family: &str| packages.join(family).join("LocalCache").join("Roaming").join("Claude");
    // Any publisher's packaged app may name itself Claude and write its own folder.
    desktop_copy(&redirected("Claude_0123456789abc"), "99.0.0", "000000000099", true, None);
    assert_eq!(find_in(Provider::Claude, &places(&t.0, &[])), None);
    let exe = desktop_copy(&redirected("Claude_pzs8sxrjxfjjc"), "2.1.325", "806e0a45e6f5", true, None);
    assert_eq!(find_in(Provider::Claude, &places(&t.0, &[])), found(&exe, Kind::DesktopApp));
    let classic = desktop_copy(&t.0.join("roaming").join("Claude"), "2.1.326", "c17a819afb40", true, None);
    assert_eq!(find_in(Provider::Claude, &places(&t.0, &[])), found(&classic, Kind::DesktopApp), "the newest version wins across both");
    // The Codex app keeps a codex.exe of its own too; it is never taken.
    file(&t.0.join("local").join("OpenAI").join("Codex").join("bin").join("c38161ad8494f88c").join("codex.exe"));
    assert_eq!(find_in(Provider::Codex, &places(&t.0, &[])), None);
}

#[test]
fn the_desktop_apps_own_executables_are_never_taken() {
    let t = TempDir::new();
    let squirrel = t.0.join("local").join("AnthropicClaude");
    let app = squirrel.join("app-5.11492.5");
    file(&squirrel.join("claude.exe"));
    file(&app.join("claude.exe"));
    file(&app.join("resources").join("app.asar"));
    // Wherever else they may be: an Electron app's folder, a Squirrel install's root, aliases.
    let electron = t.0.join("Programs").join("Claude");
    file(&electron.join("claude.exe"));
    file(&electron.join("resources").join("app.asar"));
    let root = t.0.join("Other");
    file(&root.join("claude.exe"));
    file(&root.join("Update.exe"));
    let aliases = t.0.join("local").join("Microsoft").join("WindowsApps");
    file(&aliases.join("claude.exe"));
    let p = places(&t.0, &[squirrel.clone(), app.clone(), electron, root, aliases]);
    assert_eq!(find_in(Provider::Claude, &p), None);
    // A folder PATH cannot carry, or a relative one (searched from wherever a process stands).
    let odd = t.0.join("odd;name");
    file(&odd.join("claude.exe"));
    assert_eq!(find_in(Provider::Claude, &places(&t.0, &[odd.clone()])), None);
    assert!(!carriable(Path::new(r"tools\bin")) && !carriable(Path::new(r"\tools\bin")));
    assert!(carriable(&t.0.join("chosen")) && !app_folder(&t.0.join("chosen"), Some(&t.0.join("local"))));
}

/// cmd.exe searches no PATH longer than 8,191 characters, so a child's PATH stays within that,
/// in whole entries, with the found folder first.
#[test]
fn a_childs_path_stays_within_what_cmd_searches() {
    let t = TempDir::new();
    let cli = t.0.join("cli");
    file(&cli.join("claude.exe"));
    let padding: Vec<PathBuf> = (0..400).map(|i| PathBuf::from(format!(r"C:\padding\folder-number-{i:04}\bin"))).collect();
    let mut p = places(&t.0, &[]);
    p.path = vec![cli.clone()];
    p.child = padding.iter().cloned().chain([cli.clone()]).collect();
    let path = launch_in(Provider::Claude, &p, false).path.unwrap();
    assert!(wide_len(&path) <= CMD_PATH_LIMIT, "{}", wide_len(&path));
    let dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    assert_eq!(dirs[0], cli, "the CLI's folder, past the limit in SlopBar's own order, leads");
    assert!(dirs.len() > 100 && dirs[1..].iter().all(|d| padding.contains(d)), "whole entries, in order");
    // A folder with the separator in its name goes in quoted.
    if cfg!(windows) {
        assert_eq!(join(&[PathBuf::from(r"C:\a;b"), PathBuf::from(r"C:\c")]), OsString::from(r#""C:\a;b";C:\c"#));
    }
}

/// The frontend shows the same install lines (`CLI_INSTALL` in i18n.js) as the backend.
#[test]
fn the_frontend_names_the_same_install_lines() {
    let js = include_str!("../../../src/i18n.js");
    for provider in Provider::ALL {
        let (cli, install) = install_hint(provider);
        let entry = format!("{}: [\"{cli}\", \"{}\"]", name(provider), install.replace('"', "\\\""));
        assert!(js.contains(&entry), "i18n.js CLI_INSTALL lacks {entry}");
    }
}

#[test]
fn versions_compare_as_numbers() {
    assert_eq!(version_number("2.1.325"), vec![2, 1, 325]);
    assert!(version_number("2.1.10") > version_number("2.1.9"));
    assert_eq!(version_number("2.1.4-beta.1"), vec![2, 1, 4]);
    assert_eq!(version_number("latest"), Vec::<u64>::new());
    assert!(version_number("latest") < version_number("0.0.1"));
}

#[test]
fn pathext_reads_like_cmd() {
    assert_eq!(pathext(Some(OsStr::new(".COM; .Exe;;cmd;.BAT"))), vec![".com", ".exe", ".bat"]);
    assert_eq!(pathext(Some(OsStr::new(" "))), pathext(None));
    assert_eq!(&pathext(None)[..4], [".com", ".exe", ".bat", ".cmd"]);
}

#[cfg(windows)]
#[test]
fn an_environment_blocks_variable_is_found_without_case_and_never_a_drive_entry() {
    let block: Vec<u16> = "=C:=C:\\Windows\0ALLUSERSPROFILE=C:\\ProgramData\0Path=C:\\Windows;C:\\Users\\me\\.local\\bin\0PATHEXT=.COM\0\0"
        .encode_utf16().collect();
    unsafe {
        assert_eq!(variable(block.as_ptr(), "PATH"), Some(OsString::from(r"C:\Windows;C:\Users\me\.local\bin")));
        assert_eq!(variable(block.as_ptr(), "pathext"), Some(OsString::from(".COM")));
        assert_eq!(variable(block.as_ptr(), "C:"), None);
        assert_eq!(variable(block.as_ptr(), "TEMP"), None);
    }
    let empty: Vec<u16> = vec![0, 0];
    assert_eq!(unsafe { variable(empty.as_ptr(), "Path") }, None);
}

/// Windows' own reading of the registry gives a PATH, which leads the search; a child's PATH keeps
/// this process's own order.
#[cfg(windows)]
#[test]
fn the_fresh_path_is_windows_own_and_leads_the_search_only() {
    let fresh = fresh_path().expect("CreateEnvironmentBlock gives a PATH");
    assert!(std::env::split_paths(&fresh).any(|d| key(&d).ends_with(r"\system32")), "{fresh:?}");
    let p = Places::current();
    let first = |path: &OsStr| std::env::split_paths(path).find(|d| !d.as_os_str().is_empty()).map(|d| key(&d));
    assert_eq!(p.path.first().map(|d| key(d)), first(&fresh));
    assert_eq!(p.child.first().map(|d| key(d)), first(&std::env::var_os("PATH").unwrap_or_default()));
}

/// With PATH set for a child, Rust looks for a bare program name on it first: SlopBar starts
/// Windows' own cmd.exe by its full path, never a cmd.exe planted in the CLI's folder.
#[cfg(windows)]
#[test]
fn windows_own_cmd_runs_even_with_another_cmd_on_path() {
    use std::process::Command;
    let t = TempDir::new();
    std::fs::copy(system().join("hostname.exe"), t.0.join("cmd.exe")).unwrap();
    let path = join(&[t.0.clone(), system()]);
    let out = Command::new(cmd_exe()).args(["/d", "/c", "echo windows-cmd"]).env("PATH", &path).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "windows-cmd");
    let source = crate::sources::Source { label: "test".into(), home: t.0.clone(), claude_dir: None, codex_dir: None, distro: None, native_home: t.0.to_string_lossy().into_owned() };
    let command = crate::login::command(Provider::Claude, &source, &t.0, &source.native_home, "claude --version", false);
    assert_eq!(Path::new(command.get_program()), cmd_exe());
}

/// cmd.exe runs the found file for the bare name once its folder leads PATH, even with another
/// `claude` later on PATH, through `call` in a script as `login.cmd` does, and hidden as the logout
/// and Codex's app server run; also with SlopBar's own PATH past what cmd searches.
#[cfg(windows)]
#[test]
fn cmd_runs_the_file_found_for_the_bare_name() {
    use std::process::Command;
    let t = TempDir::new();
    let (chosen, other, work) = (t.0.join("chosen"), t.0.join("other"), t.0.join("work"));
    for (dir, said) in [(&chosen, "chosen"), (&other, "other")] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("claude.cmd"), format!("@echo {said} %*\r\n")).unwrap();
    }
    std::fs::create_dir_all(&work).unwrap();
    let padding: Vec<PathBuf> = (0..400).map(|i| t.0.join(format!("padding-folder-{i:04}"))).collect();
    let mut p = places(&t.0, &[chosen.clone()]);
    p.child = [other.clone(), system()].into_iter().chain(padding).collect();
    let launch = launch_in(Provider::Claude, &p, false);
    let path = launch.path.unwrap();
    assert!(wide_len(&std::env::join_paths(&p.child).unwrap()) > CMD_PATH_LIMIT, "SlopBar's own PATH is past the limit");
    let run = |script: Option<&str>, line: &str| {
        let mut c = Command::new(cmd_exe());
        c.arg("/d").arg("/c");
        match script {
            Some(body) => {
                std::fs::write(work.join("login.cmd"), body).unwrap();
                c.arg(r".\login.cmd");
            }
            None => {
                c.arg(line);
            }
        }
        let out = c.current_dir(&work).env("PATH", &path).env("NoDefaultCurrentDirectoryInExePath", "1").output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(run(None, "claude --setting-sources user auth logout"), "chosen --setting-sources user auth logout");
    crate::login::prepare(&work, Provider::Claude, "claude --setting-sources user auth login").unwrap();
    let script = std::fs::read_to_string(work.join("login.cmd")).unwrap();
    assert_eq!(run(Some(&script), ""), "chosen --setting-sources user auth login");
    assert_eq!(run(None, "hostname >nul && echo system-found"), "system-found", "System32 is still on PATH");
}

/// The whole way for a computer whose only Claude Code is the desktop app's: found in the app's
/// folder layout, then run by cmd for the bare name. The stand-in is a copy of cmd.exe.
#[cfg(windows)]
#[test]
fn the_desktop_apps_copy_is_what_cmd_runs_for_claude() {
    use std::process::Command;
    let t = TempDir::new();
    let data = t.0.join("roaming").join("Claude");
    let exe = desktop_copy(&data, "2.1.325", "806e0a45e6f5", true, None);
    std::fs::copy(cmd_exe(), &exe).unwrap();
    let size = std::fs::metadata(&exe).unwrap().len();
    std::fs::write(exe.parent().unwrap().join(".payload"), format!("{{\"sha256\": \"\", \"size\": {size}}}")).unwrap();
    let mut p = places(&t.0, &[]);
    p.child = vec![system()];
    let launch = launch_in(Provider::Claude, &p, false);
    assert_eq!(launch.found, Some(Found { exe: exe.clone(), kind: Kind::DesktopApp }));
    let out = Command::new(cmd_exe()).args(["/d", "/c", "claude /d /c echo desktop-copy-ran"])
        .current_dir(&t.0).env("PATH", launch.path.unwrap()).envs(launch.env).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "desktop-copy-ran");
}

/// The process SlopBar starts for a sign-in (`login::command_with`, as the app builds it) runs the
/// CLI found, not another `claude` or `codex` earlier on SlopBar's own PATH: the visible sign-in's
/// `login.cmd`, and hidden as the logout and Codex's app server run; the desktop app's copy with
/// the variables the app gives it.
#[cfg(windows)]
#[test]
fn the_sign_in_runs_the_cli_found_through_login_command() {
    use std::process::Stdio;
    let t = TempDir::new();
    let (found_dir, stale) = (t.0.join("found"), t.0.join("stale"));
    for (dir, said) in [(&found_dir, "found"), (&stale, "stale")] {
        std::fs::create_dir_all(dir).unwrap();
        for cli in ["claude", "codex"] {
            std::fs::write(dir.join(format!("{cli}.cmd")), format!("@echo {said} {cli} %*\r\n")).unwrap();
        }
    }
    let source = crate::sources::Source { label: "test".into(), home: t.0.clone(), claude_dir: None, codex_dir: None, distro: None, native_home: t.0.to_string_lossy().into_owned() };
    let run = |provider: Provider, p: &Places, action: &str, visible: bool| {
        let dir = t.0.join(format!(".slopbar-login-{}-{}", name(provider), u8::from(visible)));
        std::fs::create_dir_all(&dir).unwrap();
        crate::login::prepare(&dir, provider, action).unwrap();
        let mut c = crate::login::command_with(provider, &source, &dir, &dir.to_string_lossy(), action, visible, launch_in(provider, p, false));
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        // Hidden even for the visible form: only the window differs.
        let (child, _tree) = crate::login::ProcessTree::spawn(&mut c, false).unwrap();
        String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).trim().to_string()
    };
    let mut p = places(&t.0, &[found_dir.clone()]);
    p.child = vec![stale.clone(), system()];
    assert_eq!(run(Provider::Claude, &p, "claude --setting-sources user auth login", true), "found claude --setting-sources user auth login");
    assert_eq!(run(Provider::Claude, &p, "claude --setting-sources user auth logout", false), "found claude --setting-sources user auth logout");
    assert_eq!(run(Provider::Codex, &p, "codex app-server", false), "found codex app-server");
    // The desktop app's copy (a copy of cmd.exe stands in) sees what the app sets to run it.
    let data = t.0.join("roaming").join("Claude");
    let exe = desktop_copy(&data, "2.1.325", "806e0a45e6f5", true, None);
    std::fs::copy(cmd_exe(), &exe).unwrap();
    let size = std::fs::metadata(&exe).unwrap().len();
    std::fs::write(exe.parent().unwrap().join(".payload"), format!("{{\"sha256\": \"\", \"size\": {size}}}")).unwrap();
    let mut p = places(&t.0, &[]);
    p.child = vec![stale.clone(), system()];
    assert_eq!(run(Provider::Claude, &p, "claude /d /c echo %DISABLE_AUTOUPDATER%-%NODE_USE_SYSTEM_CA%", false), "1-1");
}

/// On a computer with the Claude desktop app (run by hand: `cargo test --lib real_desktop_copy --
/// --ignored --nocapture`): with PATH and the installers' folders taken out of the search, the
/// desktop app's own Claude Code is found and runs for the bare name, and `auth login` from it
/// starts a sign-in in an isolated folder: it prints the sign-in address and waits. BROWSER is this
/// test program, which opens nothing and exits at once, and the sign-in is ended unfinished.
#[cfg(windows)]
#[test]
#[ignore]
fn real_desktop_copy_runs_claude_code() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut p = Places::current();
    p.path.clear();
    let t = TempDir::new();
    p.home = Some(t.0.join("no-home"));
    let launch = launch_in(Provider::Claude, &p, std::env::var_os("NODE_EXTRA_CA_CERTS").is_some());
    // Not a pass: an installer's folder (npm's, say) holding Claude Code comes first here.
    let Some(found) = launch.found.clone().filter(|f| f.kind == Kind::DesktopApp) else {
        eprintln!("SKIPPED: not the desktop app's copy, but {:?}", launch.found);
        return;
    };
    eprintln!("found {:?} ({:?}), with {:?}", found.exe, found.kind, launch.env);
    let path = launch.path.clone().unwrap();
    let version = Command::new(cmd_exe()).args(["/d", "/c", "claude --version"]).current_dir(&t.0).env("PATH", &path).output().unwrap();
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    eprintln!("claude --version: {version}");
    assert!(version.contains("(Claude Code)"), "{version}");

    let config = t.0.join(".slopbar-login-test");
    std::fs::create_dir(&config).unwrap();
    let mut command = Command::new(cmd_exe());
    command.args(["/d", "/c", "claude --setting-sources user auth login"])
        .current_dir(&config).env("PATH", &path).envs(launch.env)
        .env("CLAUDE_CONFIG_DIR", &config).env("CLAUDE_SECURESTORAGE_CONFIG_DIR", &config)
        .env("BROWSER", std::env::current_exe().unwrap())
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (name, _) in std::env::vars_os() {
        if crate::login::blocked_environment(&name.to_string_lossy()) {
            command.env_remove(name);
        }
    }
    let (mut child, tree) = crate::login::ProcessTree::spawn(&mut command, false).unwrap();
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = out.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
        }
        seen
    });
    std::thread::sleep(std::time::Duration::from_secs(15));
    let running = child.try_wait().unwrap().is_none();
    drop(tree);
    let _ = child.kill();
    let _ = child.wait();
    let text = String::from_utf8_lossy(&reader.join().unwrap()).to_string();
    let mut err = String::new();
    let _ = child.stderr.take().map(|mut e| e.read_to_string(&mut err));
    eprintln!("still waiting for the sign-in after 15 s: {running}\n--- output ---\n{text}\n--- errors ---\n{err}");
    assert!(running, "the sign-in ended by itself");
    let url = text.find("https://").map(|at| text[at..].split(|c: char| c.is_whitespace() || c.is_control()).next().unwrap_or_default());
    eprintln!("sign-in address: {url:?}");
    assert!(url.is_some_and(|u| crate::signin::allowed_url(u) && u.contains("oauth")), "no sign-in address in the output");
}
