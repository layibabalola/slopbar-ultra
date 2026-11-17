//! Self-update from GitHub releases (signed; see `plugins.updater` in tauri.conf.json), within the
//! deadlines and limits in `update_check`, one install at a time. Every check and install is
//! recorded for `get_app_info`.

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::update_check::{self, describe, Outcome, Steps, Trigger};
use crate::AppState;

const FIRST_CHECK: Duration = Duration::from_secs(20);
/// Why an update is put off: tokens may be rotating in the sign-in, and the installer ends SlopBar.
const SIGN_IN_OPEN: &str = "A sign-in is open; the update waits for it to finish";
/// Upstream builds look for an update every 6 hours; Ultra release builds every 15 minutes
/// (`ultra_release::CHECK_EVERY`), because their releases publish themselves.
#[cfg(not(feature = "ultra-updates"))]
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
#[cfg(feature = "ultra-updates")]
const EVERY: Duration = crate::ultra_release::CHECK_EVERY;
/// Why an automatic install waited (see `install_now`).
const SHARING_SETUP_RUNS: &str = "waiting for the sharing setup to finish";

#[derive(Clone, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    /// The newer version available, if any.
    pub latest: Option<String>,
}

/// Looks for a newer release (Settings, agents) and records what it found.
pub async fn check(app: &AppHandle) -> Result<UpdateInfo, String> {
    if !cfg!(feature = "upstream-updates") { return Err("Manual updates only in this build".into()); }
    let result = Steps::default()
        .run("The update check", update_check::CHECK_LIMIT, async {
            let current = app.package_info().version.to_string();
            let update = updater(app)?.check().await.map_err(|e| describe(&e))?;
            Ok::<_, String>(UpdateInfo { current, latest: update.map(|u| u.version) })
        })
        .await;
    record(app, Trigger::Manual, match &result {
        Ok(UpdateInfo { latest: Some(version), .. }) => Outcome::Found(version.clone()),
        Ok(_) => Outcome::UpToDate,
        Err(e) => Outcome::Failed(e.clone()),
    });
    result
}

/// The updater, with the check's request deadline and the stall limit (both through reqwest; the
/// stall limit reaches the download too, which gets its own deadline in `install_now`). Ultra
/// release builds pin the fork's channel and key in code, so a merged config change cannot point
/// them back at upstream, and accept only newer Ultra versions.
fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let h = app.clone();
    let builder = app
        .updater_builder()
        .timeout(update_check::CHECK_TIMEOUT)
        .configure_client(|client| client.read_timeout(update_check::STALL_TIMEOUT))
        .on_before_exit(move || crate::app::on_exit(&h));
    #[cfg(feature = "ultra-updates")]
    let builder = builder
        .endpoints(vec![crate::ultra_release::UPDATE_ENDPOINT.parse::<tauri::Url>().map_err(|e| e.to_string())?])
        .map_err(|e| e.to_string())?
        .pubkey(crate::ultra_release::PUBKEY)
        .version_comparator(|current, remote| {
            crate::ultra_release::is_ultra_version(&remote.version.to_string()) && remote.version > current
        });
    builder.build().map_err(|e| e.to_string())
}

/// Why an install asked for now could not start (another one runs), recorded as such; None when it
/// can start.
pub fn install_blocked(app: &AppHandle) -> Option<String> {
    let status = &app.state::<AppState>().update_check;
    let outcome = status.install_blocked()?;
    let reason = match &outcome {
        Outcome::Deferred(text) | Outcome::Failed(text) => text.clone(),
        _ => update_check::ALREADY_INSTALLING.into(),
    };
    status.record(Trigger::Manual, outcome);
    Some(reason)
}

/// Downloads and installs the newest release, then restarts into it (Settings, agents), in a task of
/// its own within the pass limit. Records what happened; an installer that starts ends this
/// process. A deferral (another install runs) comes back as an error, for the caller to show.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    if !cfg!(feature = "upstream-updates") { return Err("Manual updates only in this build".into()); }
    let h = app.clone();
    let outcome = update_check::isolated(update_check::PASS_LIMIT, move |steps| async move { install_now(&h, Trigger::Manual, &steps).await }).await;
    let result = match &outcome {
        Outcome::Failed(text) | Outcome::Deferred(text) => Err(text.clone()),
        _ => Ok(()),
    };
    record(app, Trigger::Manual, outcome);
    result
}

/// Installs the newest release, if there is one, and restarts into it, each step within its limit.
/// Returns only when there is none, when it is deferred, or on failure.
async fn install_now(app: &AppHandle, by: Trigger, steps: &Steps) -> Result<Outcome, String> {
    let found = steps
        .run("The update check", update_check::CHECK_LIMIT, async { updater(app)?.check().await.map_err(|e| describe(&e)) })
        .await?;
    let Some(mut update) = found else { return Ok(Outcome::UpToDate) };
    // On Windows the installer ends this process without an Exit event. Monitoring tokens rotate
    // only during a refresh or a sign-in, so it waits for both: any open sign-in, whatever started
    // it and in any window (`AppState::sign_in_open`), puts the update off (deferred, not failed)
    // before anything is downloaded and again before the handover, and the refresh lock goes with
    // the handover's thread, held until that thread ends: a handover that never returns keeps
    // refreshes waiting until SlopBar restarts.
    if app.state::<AppState>().sign_in_open() {
        return Ok(Outcome::Deferred(SIGN_IN_OPEN.into()));
    }
    // An automatic install waits while a sharing setup runs: restarting would end its sign-in.
    if matches!(by, Trigger::Auto) && crate::app::sharing_setup_running(app) {
        return Ok(Outcome::Deferred(SHARING_SETUP_RUNS.into()));
    }
    // One install at a time, from its download to the end of its handover (a check takes no turn).
    let turn = match app.state::<AppState>().update_check.begin_install() {
        Ok(turn) => turn,
        Err(Outcome::Failed(error)) => return Err(error),
        Err(outcome) => return Ok(outcome),
    };
    // The check's deadline does not carry over: the updater leaves the download without one.
    update.timeout = Some(update_check::DOWNLOAD_TIMEOUT);
    let bytes = steps
        .run("The download", update_check::DOWNLOAD_LIMIT, async { update.download(|_, _| {}, || {}).await.map_err(|e| describe(&e)) })
        .await?;
    let refresh_lock = app.state::<AppState>().refresh_lock.clone();
    let refresh = steps
        .run("Waiting for a refresh to finish", update_check::REFRESH_WAIT_LIMIT, async move { Ok::<_, String>(refresh_lock.lock_owned().await) })
        .await?;
    if app.state::<AppState>().sign_in_open() {
        return Ok(Outcome::Deferred(SIGN_IN_OPEN.into()));
    }
    record(app, by, Outcome::Installing(update.version.clone()));
    let _ = app.emit("update-installing", &update.version);
    // SlopBar Ultra's watchdog starts nothing while the installer replaces this executable.
    crate::keep_running::installing(&update.version);
    // On a thread of its own, holding the turn and the refresh lock until that thread ends, so no
    // refresh starts while the installer may still end this process (see `hand_over`).
    update_check::hand_over(steps, update_check::HANDOFF_LIMIT, (turn, refresh), move || update.install(bytes).map_err(|e| describe(&e))).await?;
    app.restart();
}

fn record(app: &AppHandle, by: Trigger, outcome: Outcome) {
    app.state::<AppState>().update_check.record(by, outcome);
}

/// `get_app_info`'s `update_check` (see `update_check::Status::report`).
pub fn report(app: &AppHandle) -> serde_json::Value {
    app.state::<AppState>().update_check.report(EVERY)
}

/// Checks shortly after launch and then every `EVERY`, sooner after a failure, and installs what it
/// finds while automatic updates are on (`update_check::run_loop`): each pass in a task of its own
/// within the pass limit, recorded.
pub async fn run_loop(app: AppHandle) {
    if !cfg!(feature = "upstream-updates") { return; }
    let state = app.state::<AppState>();
    let auto = || state.settings.lock().unwrap_or_else(|e| e.into_inner()).auto_update;
    let attempt = |steps: Steps| {
        let app = app.clone();
        async move { install_now(&app, Trigger::Auto, &steps).await }
    };
    update_check::run_loop(&state.update_check, FIRST_CHECK, EVERY, update_check::PASS_LIMIT, auto, attempt, tokio::time::sleep).await;
}
