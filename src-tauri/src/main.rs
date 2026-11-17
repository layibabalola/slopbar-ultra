#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    // A CLI sign-in opening its page through `BROWSER` (see `signin`): open it and exit.
    if let Some(code) = slopbar_lib::signin_launch(&args) {
        std::process::exit(code);
    }
    if args.len() == 3 && args[1] == "--build-info-to" {
        // Offline metadata only. Do not initialize Tauri or read user credentials.
        let status = slopbar_lib::write_build_info(std::path::Path::new(&args[2]));
        std::process::exit(if status.is_ok() { 0 } else { 1 });
    }
    // SlopBar Ultra's watchdog (`keep_running`): its scheduled task's check, or the uninstaller's
    // removal of that task. Before the instance claim, which would open Settings in a running app.
    if let Some(code) = slopbar_lib::keep_running(&args) {
        std::process::exit(code);
    }
    // Started by a restart: the previous instance finishes its orderly exit first. A start by the
    // watchdog claims the same way: after the check that started it, and quietly second to another.
    let watchdog = slopbar_lib::watchdog_start(&args);
    let after = watchdog.or_else(|| (args.len() == 3 && args[1] == "--after").then(|| args[2].to_str().and_then(|s| s.parse().ok())).flatten());
    // Before anything reads app data: a second instance hands over and exits.
    if !slopbar_lib::claim_instance(after) {
        return;
    }
    slopbar_lib::keep_running_started(watchdog.is_some());
    slopbar_lib::run()
}
