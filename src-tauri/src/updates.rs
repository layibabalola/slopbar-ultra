//! Self-update from GitHub releases (signed; see `plugins.updater` in tauri.conf.json). Every
//! request has a deadline, and every check and install is recorded for `get_app_info` (see
//! `update_check`).

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::update_check::{self, describe, Outcome, Trigger};
use crate::AppState;

const FIRST_CHECK: Duration = Duration::from_secs(20);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Serialize)]
pub struct UpdateInfo {
    pub current: String,
    /// The newer version available, if any.
    pub latest: Option<String>,
}

/// Looks for a newer release (Settings, agents) and records what it found.
pub async fn check(app: &AppHandle) -> Result<UpdateInfo, String> {
    if !cfg!(feature = "upstream-updates") { return Err("Manual updates only in this build".into()); }
    let result = update_check::bounded(update_check::CHECK_LIMIT, "The update check", async {
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

/// The updater, with the check's deadline (it applies to the check's request only).
fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    app.updater_builder().timeout(update_check::CHECK_TIMEOUT).build().map_err(|e| describe(&e))
}

/// Downloads and installs the newest release, then restarts into it (Settings, agents). Records
/// what happened; an installer that starts ends this process.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    if !cfg!(feature = "upstream-updates") { return Err("Manual updates only in this build".into()); }
    let result = update_check::bounded(update_check::INSTALL_LIMIT, "The update", install_now(app, Trigger::Manual)).await;
    record(app, Trigger::Manual, match &result {
        Ok(()) => Outcome::UpToDate,
        Err(e) => Outcome::Failed(e.clone()),
    });
    result
}

/// Installs the newest release, if there is one, and restarts into it. Returns only when there is
/// none (`Ok`) or on failure.
async fn install_now(app: &AppHandle, by: Trigger) -> Result<(), String> {
    let Some(mut update) = updater(app)?.check().await.map_err(|e| describe(&e))? else {
        return Ok(());
    };
    // The updater leaves the download without a deadline (the check's is not carried over).
    update.timeout = Some(update_check::DOWNLOAD_TIMEOUT);
    let bytes = update.download(|_, _| {}, || {}).await.map_err(|e| describe(&e))?;
    record(app, by, Outcome::Installing(update.version.clone()));
    let _ = app.emit("update-installing", &update.version);
    update.install(bytes).map_err(|e| describe(&e))?;
    app.restart();
}

fn record(app: &AppHandle, by: Trigger, outcome: Outcome) {
    app.state::<AppState>().update_check.record(by, outcome);
}

/// `get_app_info`'s `update_check` (see `update_check::Status::report`).
pub fn report(app: &AppHandle) -> serde_json::Value {
    app.state::<AppState>().update_check.report(EVERY)
}

/// Checks shortly after launch and then every `EVERY`, sooner after a failure; installs when
/// auto-update is on. Each pass has a deadline, so none can stall the loop, and is recorded.
pub async fn run_loop(app: AppHandle) {
    if !cfg!(feature = "upstream-updates") { return; }
    let state = app.state::<AppState>();
    state.update_check.schedule(update_check::after(FIRST_CHECK));
    tokio::time::sleep(FIRST_CHECK).await;
    let mut failures = 0;
    loop {
        let auto = state.settings.lock().unwrap_or_else(|e| e.into_inner()).auto_update;
        let wait = if auto {
            let attempt = async { install_now(&app, Trigger::Auto).await.map(|()| Outcome::UpToDate) };
            update_check::pass(&state.update_check, update_check::INSTALL_LIMIT, attempt, &mut failures, EVERY).await
        } else {
            failures = 0;
            state.update_check.schedule(update_check::after(EVERY));
            EVERY
        };
        tokio::time::sleep(wait).await;
    }
}
