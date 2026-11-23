use crate::{accounts, active_log, auto_monitor, backup, balancer, bulk, burn, calendar, codex_local, csv_export, desktop, desktop_apps, diagnostics, digest, dock, door, headroom, health, inspect, leases, ledger, migrate, model, monitoring, notify, pairing, panel, phone, pins, plan_alerts, presence, providers, rclone, refresh_wait, relay, reset_credits, roster, settings, sharing, sharing_setup, signin, sources, subscriptions, unexpected, updates, usage_index, wind_down};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::{AppHandle, Emitter, Manager, WindowEvent, Wry};
use tauri_plugin_autostart::ManagerExt;

use accounts::Provider;
use model::Snapshot;
use settings::Settings;

pub(crate) const SETTINGS_LABEL: &str = "settings";
const MENU_IDS: [&str; 3] = ["refresh", "settings", "quit"];
/// The menus' checkable item that turns the compact flyout on and off.
pub(crate) const MENU_COMPACT: &str = "compact_flyout";

pub struct AppState {
    monitor: monitoring::Monitor,
    diagnostics: std::sync::Arc<diagnostics::Recorder>,
    refresh_lock: tokio::sync::Mutex<()>,
    /// Held from reading settings to storing them, so the window and agents never lose a change.
    settings_lock: tokio::sync::Mutex<()>,
    account_operations: monitoring::Operations,
    /// Every sign-in open now, from any entry point and in any window (see `sign_in_open`).
    sign_ins: monitoring::SignIns,
    pub(crate) snapshot: Mutex<Snapshot>,
    pub(crate) settings: Mutex<Settings>,
    /// The widget's right-click menu and its items (relabeled when the language changes).
    pub(crate) menu: Mutex<Option<Menu<Wry>>>,
    pub(crate) menu_items: Mutex<Vec<MenuItem<Wry>>>,
    /// Physical center of the widget when it was last clicked; the flyout opens above it.
    pub(crate) panel_anchor: Mutex<Option<(f64, f64)>>,
    pub(crate) panel_height: Mutex<f64>,
    pub(crate) panel_hidden_at: Mutex<Option<Instant>>,
    /// Content width of the taskbar widget, in CSS px.
    pub(crate) dock_width: Mutex<f64>,
    /// Last known usage per account, for accounts whose saved token can't be used right now.
    pub(crate) usage_cache: Mutex<providers::Cache>,
    /// Changes to `usage_cache` the next refresh makes, under the refresh lock (see `retry_soon`).
    cache_due: refresh_wait::CacheDue,
    /// Every account's completed weekly windows, for the Subscriptions page.
    pub(crate) usage_history: Mutex<subscriptions::History>,
    /// The Codex CLI's own history on this PC, read in the background after each refresh.
    pub(crate) codex_local: Mutex<Option<std::sync::Arc<codex_local::Local>>>,
    /// Which account each CLI had signed in when (see `active_log`), and when it was last saved.
    pub(crate) active_log: Mutex<active_log::Log>,
    active_saved: Mutex<Option<Instant>>,
    /// Tokens by account, repository and model from the CLIs' files here, read after each refresh.
    pub(crate) usage_index: Mutex<Option<std::sync::Arc<usage_index::Index>>>,
    /// When each provider was last switched automatically (the balancer's cooldown).
    pub(crate) last_switch: Mutex<HashMap<Provider, Instant>>,
    /// A graceful switch waiting for Claude Code sessions to wrap up (see `wind_down`).
    pub(crate) wind_down: Mutex<wind_down::Tracker>,
    /// The past few hours of readings, for the agents' pace forecasts.
    pub(crate) recent: Mutex<headroom::Recent>,
    /// Sources found by the last refresh, where switches are written.
    pub(crate) sources: Mutex<Vec<sources::Source>>,
    /// Recent readings of unclaimed accounts, to notice use outside this PC.
    presence_tracks: Mutex<presence::Tracks>,
    /// The other machines' last records, for a read that misses one (see `presence::Recall`).
    presence_recall: Mutex<presence::Recall>,
    /// Where in-use status is shared: what settings saves left for the next refresh to withdraw,
    /// and the account roster's sync, its keys and last write, which share one lock (see `sharing`).
    sharing: std::sync::Arc<sharing::Sharing>,
    /// What the roster's last sync found, for Settings, Health and sign-ins.
    roster_seen: Mutex<roster::Seen>,
    /// "Sign in all accounts" as the roster carries it to the other computers (see `roster::Run`).
    roster_run: Mutex<Option<roster::Run>>,
    /// Use seen where none of the user's computers is, for a week (see `unexpected`).
    unexpected_log: Mutex<unexpected::Log>,
    /// Plan notifications already shown (renewals, running out), so each is shown once.
    plan_alerts: Mutex<plan_alerts::Sent>,
    /// Runaway agents seen in the past day (see `burn`), and notifications not yet shown.
    burn_log: Mutex<burn::Log>,
    burn_due: Mutex<Vec<burn::BurnAlert>>,
    /// What the plans have cost month by month (see `ledger`).
    ledger: Mutex<ledger::Ledger>,
    /// Connect monitoring automatically: how long accounts have been in use, and backoffs.
    auto_monitor: Mutex<auto_monitor::Tracker>,
    /// What refreshes and startup learned for Settings > Health.
    health: Mutex<HealthSeen>,
    /// Where each pinned account's project profile stood after the last sync (see `pins`).
    profile_states: Mutex<HashMap<(Provider, String), accounts::ProfileState>>,
    /// The queued "Connect monitoring for all" while it runs: the accounts left, done first.
    bulk_connect: Mutex<Option<BulkConnect>>,
    /// Stops the running one (Settings' Stop).
    bulk_stop: Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// Starts the running one's open sign-in again ("Retry this sign-in").
    bulk_restart: Mutex<Option<Arc<StepRestart>>>,
    /// Woken at every change to `bulk_connect` (see `wait_for_sign_in_change`).
    bulk_changed: tokio::sync::Notify,
    bulk_revision: std::sync::atomic::AtomicU64,
    /// Why the switcher's shortcut could not be registered (taken by another app, say).
    hotkey_error: Mutex<Option<String>>,
    /// The same for the compact toggle's shortcut.
    compact_hotkey_error: Mutex<Option<String>>,
    /// The widget menu's compact flyout item, checked while the setting is on.
    compact_item: Mutex<Option<CheckMenuItem<Wry>>>,
    /// The page Settings opens on when it is being created (it is destroyed when closed).
    settings_page: Mutex<Option<String>>,
    /// Accounts reserved for jobs (see `leases`); in memory only.
    pub(crate) leases: Mutex<leases::Leases>,
    /// The last update check and the update loop's next run (see `update_check`); in memory only.
    pub(crate) update_check: crate::update_check::Status,
    /// "Set up with Dropbox" (Settings > Data, or the door's setup_sharing): see `sharing_setup`.
    sharing_setup: Mutex<sharing_setup::Setup>,
    sharing_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// The SSH tunnel while another computer's Dropbox sign-in waits for this computer's browser.
    sharing_tunnel: Mutex<Option<sharing_setup::Tunnel>>,
    /// "Sign in all your computers from here": the plan last gathered, so the person's OK runs
    /// exactly what was shown (see `gather_everywhere`), once.
    everywhere: Mutex<Option<Everywhere>>,
    /// A sitting waiting for the person's confirmation in Settings (see `confirm_everywhere_here`):
    /// its monitoring switch, and when it was asked.
    everywhere_ask: Mutex<Option<(bool, Instant)>>,
}

impl AppState {
    /// Whether a sign-in is open now, whatever started it and whichever window its page opened in
    /// (see `any_sign_in_open`). Tokens rotate in a sign-in, and one cut off while it writes them
    /// leaves the account signed out until its owner signs in again, so whatever would cut one off
    /// can ask this first.
    // Read only where an install waits for sign-ins.
    #[allow(dead_code)]
    pub(crate) fn sign_in_open(&self) -> bool {
        any_sign_in_open(signin::any_active(), &self.account_operations, &self.sign_ins)
    }
}

/// Whether a sign-in is open, from each thing that shows one: a fresh profile in use
/// (`profile_open`, which `signin::any_active` says), a monitoring sign-in's hold on
/// `monitoring::SIGN_IN`, and the mark every sign-in holds while it is open (`sign_ins`). Only the
/// mark shows "Add account" in a private window or the default browser.
fn any_sign_in_open(profile_open: bool, operations: &monitoring::Operations, sign_ins: &monitoring::SignIns) -> bool {
    profile_open || operations.busy(monitoring::SIGN_IN) || sign_ins.any()
}

pub(crate) const SWITCHER_LABEL: &str = "switcher";

/// Progress of "Connect monitoring for all" and "Sign in all accounts", sent to Settings as
/// `bulk-monitoring`.
#[derive(Clone, Serialize)]
struct BulkConnect {
    /// When the run started, to the millisecond, which names it: a Retry from another computer, or
    /// an agent's `restart_sign_in`, acts only on the run it names (see `restartable`). Not a step's
    /// own `started_at`.
    run_started_at: chrono::DateTime<chrono::Utc>,
    total: usize,
    done: usize,
    /// The account whose sign-in is open now: sign in as this one.
    current: Option<bulk::Target>,
    /// What the open sign-in is for (None: a monitoring sign-in of "Connect monitoring for all").
    kind: Option<bulk::Kind>,
    /// "Sign in all accounts": how many emails, and which one (1-based) is signing in now.
    emails: usize,
    email: usize,
    finished: bool,
    error: Option<String>,
    /// Every sign-in of the run and how it went, kept once it ends, so an agent elsewhere can tell
    /// the person at the desktop exactly what happened.
    steps: Vec<StepRecord>,
    /// What Retry runs again: "Sign in all accounts" (`by_email`, with or without `monitoring`),
    /// else "Connect monitoring for all".
    by_email: bool,
    monitoring: bool,
    /// The open sign-in has waited long enough that the person was asked whether to retry it.
    still_waiting: bool,
    /// Bumped at every change, across runs (see `wait_for_sign_in_change`).
    revision: u64,
    /// "Sign in all your computers from here" (see `relay`): its steps include other computers'.
    everywhere: bool,
    /// The computers (or their sign-ins) it left out, before it started or as it went, with why.
    left_out: Vec<relay::Skipped>,
    /// Accounts in use on a computer whose CLI sign-in ended or ends soon: no sitting renews them.
    in_use: Vec<relay::InUse>,
}

/// "Retry this sign-in" for a run's open sign-in (Settings > Accounts, or Retry on the
/// notification that it is still waiting): it ends, its CLI with it, and a new one starts in the same
/// window, so the email's browser keeps its Google sign-in and the run goes on. Asked for one sign-in
/// of the run by its place, so an ask that comes as that one ends never starts the next one again.
#[derive(Default)]
struct StepRestart {
    asked: Mutex<Option<usize>>,
    /// When the last one was started again: a second ask right after (a double click) is the same.
    last: Mutex<Option<Instant>>,
    notify: tokio::sync::Notify,
}

/// How long after a sign-in was started again another ask for it is taken as the same one.
const RESTART_SETTLE: Duration = Duration::from_secs(5);

impl StepRestart {
    fn ask(&self, at: usize) {
        *self.asked.lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
        self.notify.notify_one();
    }

    /// Whether the run's sign-in `at` was asked for again (the ask is taken), unless it was just
    /// started again.
    fn take(&self, at: usize) -> bool {
        let mut asked = self.asked.lock().unwrap_or_else(|e| e.into_inner());
        if *asked != Some(at) {
            return false;
        }
        *asked = None;
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < RESTART_SETTLE) {
            return false;
        }
        *last = Some(Instant::now());
        true
    }

    /// A sign-in's turn begins: an ask from before it could show a page is not for it.
    fn begin(&self) {
        *self.asked.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The sign-in asked for again has started again: a second ask counts as the same one for
    /// [`RESTART_SETTLE`] from now, however long its start took.
    fn started(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
}

/// One sign-in of a run.
#[derive(Clone, Serialize)]
struct StepRecord {
    #[serde(flatten)]
    target: bulk::Target,
    kind: bulk::Kind,
    /// Another computer whose sign-in this is, completed here (see `relay`).
    #[serde(skip_serializing_if = "Option::is_none")]
    machine: Option<String>,
    state: StepState,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// When it started, when its page opened (in an email's profile, the only place that tells)
    /// and when it ended, so a run's waits can be measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StepState {
    /// Not reached: still to come, or the run stopped before it.
    Pending,
    Running,
    Done,
    /// Already done (or the account removed) by the time the run reached it.
    Skipped,
    Failed,
}

/// Outcomes `health::checks` needs that nothing else keeps.
#[derive(Clone, Default)]
struct HealthSeen {
    last_success: Option<chrono::DateTime<chrono::Utc>>,
    sharing: Option<health::SharingResult>,
    notifications_registered: Option<bool>,
}

/// Settings > Health and the agents' `get_health`: computed from state the app already has.
pub(crate) async fn health_report(app: &AppHandle) -> health::Report {
    let state = app.state::<AppState>();
    let snapshot = state.snapshot.lock().unwrap().clone();
    let settings = state.settings.lock().unwrap().clone();
    let seen = state.health.lock().unwrap().clone();
    let connected: Vec<_> = snapshot.accounts.iter().filter(|a| a.monitor_connected).map(|a| (a.provider, a.id.clone())).collect();
    let monitoring = state.monitor.health(&connected).await;
    let rclone_installed = settings.presence_remote.is_some()
        && tauri::async_runtime::spawn_blocking(crate::rclone::find).await.ok().flatten().is_some();
    #[cfg(windows)]
    let (app_enabled, system_enabled) = notify::toast_switches(&app.config().identifier);
    #[cfg(not(windows))]
    let (app_enabled, system_enabled) = (None, None);
    let n = &settings.notifications;
    let sharing = health::Sharing { folder: settings.presence_folder.clone(), remote: settings.presence_remote.clone(), rclone_installed, last: seen.sharing };
    health::checks(&health::Inputs {
        accounts: &snapshot.accounts,
        monitoring: &monitoring,
        refresh_minutes: settings.refresh_minutes,
        last_success: seen.last_success,
        door: door::health(),
        sharing: &sharing,
        roster: {
            let seen = state.roster_seen.lock().unwrap().clone();
            health::Roster { enabled: settings.account_roster, passphrase_saved: roster_passphrase_saved(), wrote: seen.wrote, read: seen.read,
                write_error: seen.write_error, read_error: seen.read_error, unreadable: seen.unreadable, waiting: roster_waiting(&state, &snapshot).len(),
                own_restores: seen.own_restores }
        },
        journal: state.diagnostics.health(),
        notifications: health::Notifications { registered: seen.notifications_registered, app_enabled, system_enabled,
            any_wanted: n.usage || n.resets || n.switches || n.renewals || n.unexpected_use || n.multi_machine },
        auto_connect: settings.auto_connect_monitoring,
        vault_plaintext: accounts::plaintext_slots(),
        processes: tauri::async_runtime::spawn_blocking(crate::instance::processes).await.ok().flatten(),
        disk: tauri::async_runtime::spawn_blocking(disk_use).await.ok().flatten(),
        wsl: sources::wsl_status(),
        other_computers: other_computers(&state),
    }, chrono::Utc::now())
}

/// How many of the user's other computers "Sign in all your computers from here" would ask (see
/// `gather_everywhere`): those paired or checked over SSH from here, and the roster's that
/// ~/.ssh/config names.
fn other_computers(state: &AppState) -> usize {
    let here = presence::identity().map(|(id, _)| id.to_string());
    let mut ids: HashSet<String> = settings::app_dir().map(|d| pairing::paired(&d)).unwrap_or_default().into_iter().map(|(id, _)| id).collect();
    let peers = state.roster_seen.lock().unwrap().peers.clone();
    if peers.iter().any(|p| !ids.contains(&p.machine_id)) {
        let aliases = ssh_aliases();
        ids.extend(peers.into_iter().filter(|p| aliases.contains(&p.machine.trim().to_lowercase())).map(|p| p.machine_id));
    }
    ids.into_iter().filter(|id| Some(id) != here.as_ref()).count()
}

/// Space on disk for Health, measured at most every few minutes: the flyout asks after every
/// refresh, and walking the folders each time would be wasted work.
fn disk_use() -> Option<health::Disk> {
    static LAST: Mutex<Option<(Instant, health::Disk)>> = Mutex::new(None);
    const FRESH: Duration = Duration::from_secs(300);
    if let Some((at, disk)) = *LAST.lock().unwrap() {
        if at.elapsed() < FRESH {
            return Some(disk);
        }
    }
    let data = settings::app_dir()?;
    let rollback = dirs::data_local_dir()?.join("SlopBar-Rollback");
    let disk = health::Disk::measure(&data, &rollback);
    *LAST.lock().unwrap() = Some((Instant::now(), disk));
    Some(disk)
}

#[tauri::command]
async fn get_health(app: AppHandle) -> health::Report {
    health_report(&app).await
}

/// Quits, then starts the installed executable again (the door's `restart_app`, Health's fix).
fn restart(app: &AppHandle) -> Result<(), String> {
    // One successor however many callers ask at once (parallel installs each ask).
    static RESTARTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RESTARTING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return Ok(());
    }
    // The installed executable, even if this one was renamed aside for an update.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?
        .with_file_name(crate::instance::APP_EXE);
    // A duplicate of an older build ignores the instance lock and would take the door over once
    // this one exits; it renews the same monitoring sessions, so it is ended rather than kept.
    crate::instance::end_duplicates();
    if let Err(e) = std::process::Command::new(&exe).arg("--after").arg(std::process::id().to_string()).spawn() {
        RESTARTING.store(false, std::sync::atomic::Ordering::Release);
        return Err(format!("restart failed: {e}"));
    }
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        h.exit(0);
    });
    Ok(())
}

#[tauri::command]
fn restart_app(app: AppHandle) -> Result<(), String> {
    restart(&app)
}

#[tauri::command]
fn get_usage(state: tauri::State<AppState>) -> Snapshot {
    state.snapshot.lock().unwrap().clone()
}

fn subscription_report(state: &AppState) -> subscriptions::Report {
    let snapshot = state.snapshot.lock().unwrap().clone();
    let settings = state.settings.lock().unwrap().clone();
    let local = state.codex_local.lock().unwrap().clone();
    let now = chrono::Utc::now();
    let mut report = subscriptions::report(&snapshot, &state.usage_history.lock().unwrap(), &settings, local.as_deref(), now);
    report.spend = Some(ledger::summary(&state.ledger.lock().unwrap(), &settings.spend, now));
    report
}

/// Charges renewals that have passed and this month's extra usage to the spend ledger.
fn record_spend(state: &AppState) {
    let report = subscription_report(state);
    let ledger = {
        let mut ledger = state.ledger.lock().unwrap();
        ledger::record(&mut ledger, &report, chrono::Utc::now()).then(|| ledger.clone())
    };
    if let Some(ledger) = ledger { let _ = ledger::save(&ledger); }
}

#[tauri::command]
fn get_subscriptions(state: tauri::State<AppState>) -> subscriptions::Report {
    subscription_report(&state)
}

/// Each account's weekly usage over the last 7 days, for the sparklines (see
/// `subscriptions::sparklines`), keyed `provider:id`.
#[tauri::command]
fn get_sparklines(state: tauri::State<AppState>) -> HashMap<String, Vec<(i64, f64)>> {
    subscriptions::sparklines(&state.usage_history.lock().unwrap(), chrono::Utc::now())
}

/// Esc in the flyout closes it.
#[tauri::command]
fn hide_panel(app: AppHandle) {
    panel::close(&app);
}

/// The plan notifications due now (see `plan_alerts`), each returned once. The app asks after
/// every refresh and words them (see `alerts`).
#[tauri::command]
fn take_plan_alerts(state: tauri::State<AppState>) -> Vec<plan_alerts::Alert> {
    plan_alerts_due(&state)
}

pub(crate) fn plan_alerts_due(state: &AppState) -> Vec<plan_alerts::Alert> {
    let snapshot = state.snapshot.lock().unwrap().clone();
    if snapshot.updated_at.is_none() { return vec![]; }
    let report = subscription_report(state);
    let prefs = state.settings.lock().unwrap().notifications.clone();
    let mut sent = state.plan_alerts.lock().unwrap();
    let due = plan_alerts::due(&report, &snapshot, &prefs, &mut sent, chrono::Utc::now());
    // Unsaved, they are still remembered until the app restarts.
    if !due.is_empty() { let _ = plan_alerts::save(&sent); }
    due
}

/// Runaway-agent (see `burn`) and reset-credit (see `reset_credits`) notifications due now, each
/// returned once. The app asks after every refresh and words them (see `alerts`).
#[tauri::command]
fn take_capacity_alerts(state: tauri::State<AppState>) -> serde_json::Value {
    capacity_alerts_due(&state)
}

pub(crate) fn capacity_alerts_due(state: &AppState) -> serde_json::Value {
    let burn = std::mem::take(&mut *state.burn_due.lock().unwrap());
    let snapshot = state.snapshot.lock().unwrap().clone();
    let enabled = state.settings.lock().unwrap().notifications.reset_credits;
    let credits = if snapshot.updated_at.is_some() {
        let mut sent = state.plan_alerts.lock().unwrap();
        let due = reset_credits::due(&snapshot.accounts, enabled, &mut sent, chrono::Utc::now());
        if !due.is_empty() { let _ = plan_alerts::save(&sent); }
        due
    } else { vec![] };
    serde_json::json!({ "burn": burn, "credits": credits })
}

/// Spend or switch, per Codex account at its limit with a reset credit, for the flyout.
#[tauri::command]
fn get_credit_advice(state: tauri::State<AppState>) -> Vec<reset_credits::CreditAdvice> {
    reset_credits::advise(&state.snapshot.lock().unwrap().accounts, chrono::Utc::now())
}

/// The Monday digest (see `digest`), once per week. The app asks after every refresh and words
/// it (see `alerts`).
#[tauri::command]
fn take_digest(state: tauri::State<AppState>) -> Option<digest::Digest> {
    digest_due(&state)
}

pub(crate) fn digest_due(state: &AppState) -> Option<digest::Digest> {
    if state.snapshot.lock().unwrap().updated_at.is_none() { return None; }
    let enabled = state.settings.lock().unwrap().notifications.digest;
    let local = chrono::Local::now().naive_local();
    digest::week_due(local)?;
    let report = subscription_report(state);
    let history = state.usage_history.lock().unwrap().clone();
    let mut sent = state.plan_alerts.lock().unwrap();
    let digest = digest::take(&report, &history, enabled, &mut sent, local, chrono::Utc::now());
    if digest.is_some() { let _ = plan_alerts::save(&sent); }
    digest
}

/// Writes the plan-renewal calendar now (see `calendar`); shared by Settings, the agents' door and
/// each refresh while it is turned on.
async fn write_calendar(app: &AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let report = subscription_report(&state);
    let settings = state.settings.lock().unwrap().clone();
    let (path, events) = tauri::async_runtime::spawn_blocking(move || calendar::export(&report, &settings, chrono::Utc::now()))
        .await.map_err(|e| e.to_string())??;
    Ok(serde_json::json!({ "path": path, "events": events }))
}

#[tauri::command]
async fn export_calendar(app: AppHandle) -> Result<serde_json::Value, String> {
    write_calendar(&app).await
}

/// Whether Settings may store `path` as the calendar's path.
#[tauri::command]
fn check_calendar_path(path: String) -> bool {
    calendar::valid_path(path.trim())
}

/// Where the calendar goes when no path is set.
#[tauri::command]
fn default_calendar_path() -> Option<String> {
    calendar::default_path().map(|p| p.to_string_lossy().into_owned())
}

/// Opens the provider's billing page in the browser. The address is fixed per provider, so the
/// window can only choose which of the two opens.
#[tauri::command]
async fn open_billing_page(provider: Provider) -> Result<(), String> {
    let url = subscriptions::billing_page(provider);
    tauri::async_runtime::spawn_blocking(move || std::process::Command::new("explorer.exe").arg(url).spawn().map(|_| ()))
        .await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())
}

/// Writes a diagnostics export; shared by Settings and the agents' door.
async fn write_export(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let state = app.state::<AppState>();
    let recorder = state.diagnostics.clone();
    let snapshot = state.snapshot.lock().unwrap().clone();
    let settings = state.settings.lock().unwrap().clone();
    tauri::async_runtime::spawn_blocking(move || recorder.export(&snapshot, &settings))
        .await.map_err(|_| "diagnostic_storage")?.map_err(String::from)
}

#[tauri::command]
async fn export_diagnostics(app: AppHandle) -> Result<String, String> {
    let path = write_export(&app).await?;
    #[cfg(windows)]
    if let Some(parent) = path.parent() {
        let folder = parent.to_owned();
        // No shell or user-supplied command. Open the dedicated export folder.
        let _ = tauri::async_runtime::spawn_blocking(move || std::process::Command::new("explorer.exe").arg(folder).spawn()).await;
    }
    Ok(path.to_string_lossy().into_owned())
}

/// An agent's switch: the same as Use in Settings, then the balancer's cooldown so the next poll
/// does not switch straight back. Returns the new snapshot and whether the account held.
async fn agent_switch(app: &AppHandle, provider: Provider, id: String) -> Result<serde_json::Value, String> {
    // A saved CLI sign-in that had ended before the switch: the CLI stops until it signs in again.
    let ended_at = app.state::<AppState>().snapshot.lock().unwrap().accounts.iter()
        .find(|a| a.provider == provider && a.id == id && !a.active)
        .and_then(|a| a.cli_ends_at).filter(|t| *t <= chrono::Utc::now());
    switch_to(app, provider, id.clone(), false).await?;
    app.state::<AppState>().last_switch.lock().unwrap().insert(provider, Instant::now());
    let desktop = switch_desktop(app, provider, &id).await;
    // A refresh tells whether the switch held; one that gave up waiting cannot tell (null).
    let refreshed = refresh_within(app).await;
    let held = refresh_wait::switch_held(refreshed.as_ref(), provider, &id);
    let snapshot = refreshed.unwrap_or_else(|| app.state::<AppState>().snapshot.lock().unwrap().clone());
    let mut result = serde_json::to_value(snapshot).map_err(|e| e.to_string())?;
    result["switched_to_requested"] = held.into();
    result["desktop"] = serde_json::to_value(desktop).map_err(|e| e.to_string())?;
    if let Some(at) = ended_at {
        result["cli_signin_ended_at"] = serde_json::json!(at);
        result["warning"] = "This account's saved CLI sign-in had ended, so the CLI stops until it signs in again: the user runs `claude /login`, or switches back. Next time, sign_in_all signs it in again first, without switching.".into();
    }
    Ok(result)
}

/// Bumped by every guided desktop switch, so an older one stops watching.
static DESKTOP_GUIDE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// After a manual switch to `id`: moves this machine's desktop app onto it too, when the user
/// turned that on, by the only supported means (see `desktop_apps`). Claude Desktop is opened for
/// the user to sign in with the named account and watched until it reports it; the Codex app
/// already shares the switched sign-in and only needs a restart, which is left to the user.
async fn switch_desktop(app: &AppHandle, provider: Provider, id: &str) -> desktop_apps::DesktopSwitch {
    use desktop_apps::{DesktopSwitch, Signed};
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    if !settings.switch_desktop {
        return DesktopSwitch::Off;
    }
    // Whatever this switch does, an older guided one stops watching.
    let generation = DESKTOP_GUIDE.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let apps = tauri::async_runtime::spawn_blocking(desktop_apps::read).await.unwrap_or_default();
    let lang = crate::i18n::resolve(&settings.language);
    let name = account_label(app, provider, id).unwrap_or_else(|| crate::i18n::t(&lang, "unknownAccount", &[]));
    match apps.get(provider) {
        None => DesktopSwitch::NotInstalled,
        Some(Signed::SharesCli) => {
            let _ = crate::notify::notify(app.clone(), crate::i18n::t(&lang, "codexRestartTitle", &[("name", &name)]),
                crate::i18n::t(&lang, "codexRestartBody", &[("name", &name)]), None, None);
            DesktopSwitch::Shared
        }
        Some(Signed::ClaudeAccount(current)) => {
            let Some(uuid) = id.split(':').next().filter(|u| !id.starts_with("token:") && !u.is_empty()).map(String::from) else {
                return DesktopSwitch::NotInstalled;
            };
            if *current == uuid {
                return DesktopSwitch::Already;
            }
            let _ = desktop_apps::open_claude();
            let _ = crate::notify::notify(app.clone(), crate::i18n::t(&lang, "desktopGuideTitle", &[("name", &name)]),
                crate::i18n::t(&lang, "desktopGuideBody", &[("name", &name)]), None, None);
            let (watcher, done_name) = (app.clone(), name.clone());
            tauri::async_runtime::spawn(async move {
                let cancelled = move || DESKTOP_GUIDE.load(std::sync::atomic::Ordering::SeqCst) != generation;
                let signed = tauri::async_runtime::spawn_blocking(move || desktop_apps::wait_for_claude(&uuid, &cancelled))
                    .await.unwrap_or(false);
                if signed {
                    let lang = crate::i18n::resolve(&watcher.state::<AppState>().settings.lock().unwrap().language);
                    let _ = crate::notify::notify(watcher.clone(), crate::i18n::t(&lang, "desktopGuideDone", &[("name", &done_name)]), String::new(), None, None);
                    do_refresh(&watcher).await;
                }
            });
            DesktopSwitch::Guided { expected: name }
        }
    }
}

/// `door_call`, whose answer says so when a refresh it needed gave up waiting for the refresh lock:
/// its numbers are then the last ones (`refresh_wait::mark_waiting`).
async fn door_reply(app: AppHandle, request: door::Request) -> Result<serde_json::Value, String> {
    refresh_wait::noted(door_call(app, request)).await
}

/// One operation for an agent, through the door (see `door` and `mcp`). Each runs the same code as
/// the matching Settings action, so the windows and journal see it the same way.
async fn door_call(app: AppHandle, request: door::Request) -> Result<serde_json::Value, String> {
    use door::Request as R;
    fn value(x: impl Serialize) -> Result<serde_json::Value, String> {
        serde_json::to_value(x).map_err(|e| e.to_string())
    }
    match request.clone() {
        R::ExportDiagnostics => Ok(serde_json::json!({ "path": write_export(&app).await? })),
        R::GetUsage => value(app.state::<AppState>().snapshot.lock().unwrap().clone()),
        R::Refresh => value(do_refresh(&app).await),
        R::GetSettings => value(app.state::<AppState>().settings.lock().unwrap().clone()),
        R::GetSubscriptions => value(subscription_report(&app.state::<AppState>())),
        R::GetPlanAdvice => {
            let report = subscription_report(&app.state::<AppState>());
            let credits = reset_credits::advise(&app.state::<AppState>().snapshot.lock().unwrap().accounts, chrono::Utc::now());
            Ok(serde_json::json!({ "providers": report.advice, "reset_credits": credits, "generated_at": report.generated_at }))
        }
        R::GetBurnAlerts => {
            let state = app.state::<AppState>();
            let settings = state.settings.lock().unwrap().clone();
            let alerts = burn::seen(&state.burn_log.lock().unwrap(), chrono::Utc::now());
            Ok(serde_json::json!({ "alerts": value(alerts)?, "thresholds": settings.burn, "notify": settings.notifications.burn, "notify_every_hours": burn::NOTIFY_EVERY_H }))
        }
        R::GetUsageBreakdown { provider, account, days, group_by } => {
            if days.is_some_and(|d| !(1..=usage_index::LOOKBACK_DAYS as u32).contains(&d)) {
                return Err(format!("days must be 1-{}", usage_index::LOOKBACK_DAYS));
            }
            value(usage_breakdown(&app, usage_index::Filter { provider, account, days: days.unwrap_or(7), group_by: group_by.unwrap_or_default() }).await)
        }
        R::ExportCsv { folder } => value(write_csv(&app, folder).await?),
        R::ExportBackup { path, passphrase } => write_backup(&app, path, passphrase).await,
        R::RestoreBackup { path, passphrase, dry_run } => {
            let bytes = tauri::async_runtime::spawn_blocking(move || backup::read(&path)).await.map_err(|_| "backup_unreadable".to_string())??;
            value(apply_backup(&app, bytes, passphrase, dry_run).await?)
        }
        R::UpdateSettings { patch } => {
            let state = app.state::<AppState>();
            let held = state.settings_lock.lock().await;
            let current = app.state::<AppState>().settings.lock().unwrap().clone();
            let next = settings::patched(&current, &patch, updates_enabled())?;
            let snapshot = app.state::<AppState>().snapshot.lock().unwrap().clone();
            for key in next.aliases.keys().filter(|k| !current.aliases.contains_key(*k)) {
                if !snapshot.accounts.iter().any(|a| format!("{}:{}", a.provider.name(), a.id) == *key) {
                    return Err(no_alias_account(key));
                }
            }
            if store_settings(&app, held, next.clone())? {
                do_refresh(&app).await;
            }
            value(next)
        }
        R::SetupSharing { cancel: true, .. } => value(cancel_sharing_setup_now(&app)),
        R::SetupSharing { remote, via_tunnel, reconnect, .. } => value(start_sharing_setup(&app, remote, via_tunnel, reconnect)?),
        R::GetSharingSetup { wait_seconds, since } => {
            let state = app.state::<AppState>();
            if let Some(wait) = wait_seconds {
                // Waits here, so an agent following another computer's setup asks over SSH seldom.
                let since = since.unwrap_or_else(|| state.sharing_setup.lock().unwrap().revision);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(u64::from(wait.min(45)));
                loop {
                    let changed = state.sharing_setup.lock().unwrap().revision != since;
                    if changed || tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            let setup = state.sharing_setup.lock().unwrap().clone();
            value(setup)
        }
        R::SharingSetupLink => {
            let setup = app.state::<AppState>().sharing_setup.lock().unwrap().clone();
            Ok(serde_json::json!({ "phase": setup.phase, "error": setup.error, "state": setup.link.as_deref().and_then(sharing_setup::state_of) }))
        }
        R::SharingTunnel { host, state, close } => sharing_tunnel(&app, host, state, close).await,
        R::SwitchAccount { provider, id } => agent_switch(&app, provider, id).await,
        R::GetHeadroom { provider, id } => {
            let state = app.state::<AppState>();
            let snapshot = state.snapshot.lock().unwrap().clone();
            let settings = state.settings.lock().unwrap().clone();
            let now = chrono::Utc::now();
            let picked: Vec<_> = snapshot.accounts.iter().filter(|a| match (&provider, &id) {
                (_, Some(id)) => a.id == *id && provider.map_or(true, |p| a.provider == p),
                (Some(p), None) => a.provider == *p && a.active,
                (None, None) => a.active,
            }).collect();
            if picked.is_empty() {
                return Err(if id.is_some() { "monitor_wrong_account".into() } else { "no_active_account".into() });
            }
            let accounts: Vec<_> = {
                let recent = state.recent.lock().unwrap();
                picked.into_iter().map(|a| headroom::headroom(a, &settings, &recent, now)).collect()
            };
            Ok(serde_json::json!({ "accounts": value(accounts)?, "updated_at": snapshot.updated_at }))
        }
        R::RecommendAccount { provider, expected_percent, hours, switch, lease_id } => {
            if expected_percent.is_some_and(|p| !(0.0..=100.0).contains(&p)) || hours.is_some_and(|h| !(0.0..=168.0).contains(&h)) {
                return Err("expected_percent must be 0-100 and hours 0-168".into());
            }
            let rec = recommend_unleased(&app, provider, expected_percent, hours, lease_id.as_deref());
            let mut result = value(&rec)?;
            // Here only: pick_account's and lease_account's errors quote the summary with no excluded list.
            result["summary"] = format!("{}{}", rec.summary, headroom::shared_note(&rec)).into();
            result["switched"] = false.into();
            if let (true, headroom::Action::Switch, Some(id)) = (switch, rec.action, rec.recommended.clone()) {
                let done = agent_switch(&app, provider, id).await?;
                result["switched"] = true.into();
                result["switched_to_requested"] = done["switched_to_requested"].clone();
            }
            Ok(result)
        }
        R::LeaseAccount { provider, purpose, minutes, expected_percent, id } => {
            if expected_percent.is_some_and(|p| !(0.0..=100.0).contains(&p)) {
                return Err("expected_percent must be 0-100".into());
            }
            lease_account(&app, provider, &purpose, minutes, expected_percent, id, None).await
        }
        R::ReleaseLease { lease_id } => {
            let released = app.state::<AppState>().leases.lock().unwrap().release(&lease_id).ok_or("no_such_lease")?;
            leases_changed(&app).await;
            Ok(serde_json::json!({ "released": true, "lease": value(released)? }))
        }
        R::ListLeases => {
            prune_leases(&app).await;
            Ok(serde_json::json!({ "leases": value(app.state::<AppState>().leases.lock().unwrap().list())? }))
        }
        R::PickAccount { provider, pid, purpose, expected_percent } => {
            if expected_percent.is_some_and(|p| !(0.0..=100.0).contains(&p)) {
                return Err("expected_percent must be 0-100".into());
            }
            if !leases::process_alive(pid) {
                return Err("pid_not_running".into());
            }
            prune_leases(&app).await;
            let rec = recommend_unleased(&app, provider, expected_percent, None, None);
            let Some(id) = rec.recommended.clone() else { return Err(format!("no_account_free: {}", rec.summary)) };
            let label = rec.recommended_label.clone();
            let at_default = rec.ranking.iter().any(|c| c.id == id && c.active);
            if at_default && purpose.is_none() {
                return Ok(serde_json::json!({ "id": id, "label": label, "dir": null, "env_var": pins::env_var(provider), "lease_id": null, "note": null }));
            }
            let mut leased = lease_account(&app, provider, purpose.as_deref().unwrap_or("slopbar-run"), None, expected_percent, Some(id), Some(pid)).await?;
            leased["dir"] = leased["config_dir"].clone();
            leased["lease_id"] = leased["lease"]["lease_id"].clone();
            leased["note"] = if !at_default && leased["dir"].is_null() {
                "the recommended account's profile is not signed in yet, so this runs on the CLI's current sign-in".into()
            } else {
                serde_json::Value::Null
            };
            Ok(leased)
        }
        R::RecordSession { session_id, cwd, config_dir } => record_session(&app, session_id, cwd, config_dir).await,
        R::Turn { session_id, event, config_dir } => record_turn(&app, session_id, event, config_dir).await,
        R::AddAccount { provider } => {
            desktop_shown()?;
            // One pending sign-in per provider, so a retrying agent cannot flood the desktop.
            let pending = app.state::<AppState>().account_operations.begin(provider, "add-account")?;
            start_add_account(app.clone(), provider, Some(pending)).await?;
            Ok(serde_json::json!({ "started": true, "next": "The user completes the sign-in in the terminal that opened; the account then appears in get_usage." }))
        }
        R::ConnectMonitoring { provider, id } => {
            desktop_shown()?;
            connect_monitoring(app.clone(), provider, id).await?;
            Ok(serde_json::json!({ "started": true, "next": "The user completes the monitoring sign-in that opened; get_usage then shows monitor_connected, and no monitor_renewal." }))
        }
        R::DisconnectMonitoring { provider, id } => value(disconnect_monitoring(app.clone(), provider, id).await?),
        R::RemoveAccount { provider, id } => value(remove_account(app.clone(), provider, id).await?),
        R::RevokeAccountSessions { provider, id, confirm, remove_slot } => {
            if !confirm {
                let (connected, active) = session_facts(&app, provider, &id).ok_or("monitor_wrong_account")?;
                return Ok(serde_json::json!({ "revoked": false, "monitoring_connected": connected, "in_use": active,
                    "would_revoke_monitoring": connected, "would_remove_slot": remove_slot && !active, "cli_login": "managed_by_cli",
                    "next": "Nothing was revoked. confirm: true revokes SlopBar's monitoring sign-in for this account at the provider and forgets it here; remove_slot: true also removes the saved CLI login from SlopBar (not while in use). The CLI's own sign-in is managed by the CLI: sign it out there (claude /logout, codex logout)." }));
            }
            value(revoke_sessions(&app, provider, &id, remove_slot).await?)
        }
        R::ConnectMonitoringAll { confirm } => {
            if !confirm {
                let to_connect = bulk::to_connect(&app.state::<AppState>().snapshot.lock().unwrap());
                return Ok(serde_json::json!({ "started": false, "to_connect": to_connect,
                    "next": "Nothing was started. With confirm: true the sign-ins open one at a time; the user signs in as each account named here." }));
            }
            desktop_shown()?;
            let queued = connect_all(&app).await?;
            Ok(serde_json::json!({ "started": !queued.is_empty(), "queued": queued,
                "next": "Each sign-in opens after the previous one finishes; the user signs in as the account named in Settings. get_usage shows monitor_connected, and no monitor_renewal, as they complete." }))
        }
        R::SignInAll { everywhere: true, confirm, monitoring, retry: _ } => {
            if !confirm {
                let plan = gather_everywhere(&app, monitoring).await?;
                let mut shown = value(&plan)?;
                shown["started"] = false.into();
                shown["next"] = EVERYWHERE_PREVIEW.into();
                return Ok(shown);
            }
            desktop_shown()?;
            // One run at a time: an open sign-in is never stopped for an agent's request.
            if app.state::<AppState>().bulk_connect.lock().unwrap().as_ref().is_some_and(|b| !b.finished) {
                return Err("signin_busy".into());
            }
            let plan = gather_everywhere(&app, monitoring).await?;
            let any = !plan.computers.is_empty();
            let asked = any && ask_everywhere(&app, &plan);
            let mut shown = value(&plan)?;
            shown["started"] = false.into();
            shown["asked"] = asked.into();
            shown["next"] = if asked { EVERYWHERE_ASKED } else if any { EVERYWHERE_UNASKED } else { EVERYWHERE_NOTHING }.into();
            Ok(shown)
        }
        R::SignInAll { confirm, monitoring, retry, everywhere: false } => {
            if !confirm {
                return Ok(serde_json::json!({ "started": false, "plan": sign_in_plan(&app, monitoring),
                    "next": "Nothing was started. With confirm: true the sign-ins open one at a time, an email's back to back in one fresh browser profile that is closed and deleted when the email is done; the user signs in as each account named here and completes every page." }));
            }
            desktop_shown()?;
            // Retry from another computer: a run still going (one left waiting) gives way first,
            // but never one signing in every computer from here (its pages open here).
            if retry {
                if app.state::<AppState>().bulk_connect.lock().unwrap().as_ref().is_some_and(|b| !b.finished && b.everywhere) {
                    return Err("signin_busy".into());
                }
                stop_and_wait(&app).await?;
            }
            let plan = sign_in_all(&app, monitoring).await?;
            Ok(serde_json::json!({ "started": !plan.is_empty(), "plan": plan,
                "next": "Each sign-in opens after the previous one finishes. get_usage shows the accounts, monitor_connected, and no monitor_renewal or cli_renewal, as they complete." }))
        }
        R::RestartSignIn { step, run_started_at } => {
            // Its new page opens on this desktop, as every sign-in started through the door.
            desktop_shown()?;
            Ok(match restart_from_elsewhere(&app, step, run_started_at) {
                Ok(()) => serde_json::json!({ "restarted": true, "step": step, "next": RESTARTED_NEXT }),
                Err(why) => serde_json::json!({ "restarted": false, "reason": why, "next": not_restarted_next(why) }),
            })
        }
        R::RelayPlan { monitoring, from } => Ok(relay_plan(&app, monitoring, &from).await),
        R::RelayStart { step, from, machine, kind, provider, id, email } => relay_start(&app, step, from, machine, kind, provider, id, email).await,
        R::RelayState { step, since, wait_seconds } => {
            let wait = Duration::from_secs(u64::from(wait_seconds.min(40)));
            relay::SLOT.wait_state(&step, since, wait).await.map_or_else(|| Err("relay_step_unknown".into()), value)
        }
        R::RelayStop { step } => Ok(serde_json::json!({ "stopped": relay::SLOT.stop(&step) })),
        R::RelayPage { nonce, url } => {
            if relay::SLOT.deposit(&nonce, &url) { Ok(serde_json::json!({ "taken": true })) } else { Err("signin_url_rejected".into()) }
        }
        R::RemoveLapsedAccounts { confirm } => {
            if !confirm {
                let state = app.state::<AppState>();
                let snapshot = state.snapshot.lock().unwrap().clone();
                let settings = state.settings.lock().unwrap().clone();
                let plan = bulk::lapsed(&snapshot, &settings, chrono::Utc::now());
                return Ok(serde_json::json!({ "removed": false, "to_remove": plan.remove, "in_use": plan.in_use,
                    "next": "Nothing was removed. confirm: true removes to_remove and revokes their monitoring sign-ins; accounts in use are never removed." }));
            }
            Ok(remove_lapsed(&app).await)
        }
        R::ListProjectPins => value(pin_listing(&app)),
        R::SetProjectPin { folder, provider, id } => value(set_project_pin(app.clone(), folder, provider, id).await?),
        R::CheckForUpdates => value(check_for_updates(app.clone()).await?),
        R::InstallUpdate => {
            // One install at a time: a request while one runs hears so at once.
            if let Some(reason) = updates::install_blocked(&app) {
                return Ok(serde_json::json!({ "installing": false, "reason": reason }));
            }
            let latest = check_for_updates(app.clone()).await?.latest;
            if latest.is_none() {
                return Ok(serde_json::json!({ "installing": false, "reason": "SlopBar is up to date" }));
            }
            // Installing restarts SlopBar: reply first, then install.
            let h = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = updates::install(&h).await;
            });
            Ok(serde_json::json!({ "installing": true, "version": latest, "next": "SlopBar restarts into the new version shortly." }))
        }
        R::GetAppInfo => Ok(serde_json::json!({
            "version": app_version(app.clone()),
            "updates_enabled": updates_enabled(),
            // The last update check's time, result and error, and when the update loop runs next.
            "update_check": updates::report(&app),
            "build": diagnostics::build_info(),
            // Which process answers the door (the installer quits exactly this one).
            "pid": std::process::id(),
        })),
        R::InspectWindow { window, page, screenshot } => {
            let target = match window {
                door::InspectTarget::Settings => inspect::Target::Settings,
                door::InspectTarget::Flyout => inspect::Target::Flyout,
                door::InspectTarget::Widget => inspect::Target::Widget,
                // Created for the look if it is closed (see inspect::inspect).
                door::InspectTarget::Switcher => inspect::Target::Switcher,
            };
            value(inspect::inspect(&app, target, page, screenshot).await?)
        }
        R::GetHealth => value(health_report(&app).await),
        R::GetRoster { sync } => Ok(roster_status(&app, sync).await),
        R::SyncRoster => Ok(roster_status(&app, true).await),
        R::StopSignIns => {
            let state = app.state::<AppState>();
            let running = state.bulk_connect.lock().unwrap().as_ref().is_some_and(|b| !b.finished);
            stop_running_sign_ins(&state);
            // A sign-in another computer is completing in its browser stops too.
            let relayed = relay::SLOT.stop_any();
            Ok(serde_json::json!({ "stopped": running || relayed }))
        }
        R::GetReadiness { wait_seconds, since } => {
            if let Some(wait) = wait_seconds {
                let since = since.unwrap_or_else(|| app.state::<AppState>().bulk_revision.load(std::sync::atomic::Ordering::SeqCst));
                wait_for_sign_in_change(&app, since, Duration::from_secs(u64::from(wait.min(45)))).await;
            }
            Ok(readiness(&app).await)
        }
        R::SetRosterPassphrase { passphrase } => set_roster_passphrase(&app, passphrase).await,
        R::ListSharingLocations => {
            let (folder, remote) = {
                let s = app.state::<AppState>().settings.lock().unwrap().clone();
                (s.presence_folder, s.presence_remote)
            };
            Ok(serde_json::json!({
                "folders": sync_folders().await,
                "rclone": rclone_remotes().await,
                "current": { "presence_folder": folder, "presence_remote": remote },
                "next": "Set presence_remote (an rclone remote name, no colon) or presence_folder (a folder path) with update_settings.",
            }))
        }
        R::OpenBillingPage { provider } => {
            open_billing_page(provider).await?;
            Ok(serde_json::json!({ "opened": subscriptions::billing_page(provider) }))
        }
        R::ExportCalendar => write_calendar(&app).await,
        R::OpenSettings => {
            show_settings(&app);
            Ok(serde_json::json!({ "opened": true }))
        }
        R::QuitApp | R::RestartApp => {
            let restart = matches!(request, R::RestartApp);
            // Reply first, then exit exactly as the tray's Quit does (journal flushed on Exit).
            if restart {
                self::restart(&app)?;
            } else {
                let h = app.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    h.exit(0);
                });
            }
            Ok(serde_json::json!({ "quitting": true, "restarting": restart }))
        }
    }
}

#[tauri::command]
fn record_frontend_error(state: tauri::State<AppState>, surface: diagnostics::Surface, kind: diagnostics::FrontendKind, line: Option<u32>, column: Option<u32>) {
    state.diagnostics.record(None, None, diagnostics::Event::Frontend { surface, kind, line, column });
}

#[tauri::command]
async fn refresh(app: AppHandle) -> Snapshot {
    do_refresh(&app).await
}

/// Settings > Data, "Rescan WSL": reads every distro once, starting stopped ones, then refreshes.
#[tauri::command]
async fn rescan_wsl(app: AppHandle) -> Snapshot {
    sources::request_wsl_rescan();
    do_refresh(&app).await
}

#[derive(Clone, Serialize)]
struct Switched {
    provider: Provider,
    from: Option<String>,
    to: Option<String>,
    auto: bool,
}

/// Display name of an account: its alias, else its email.
fn account_label(app: &AppHandle, provider: Provider, id: &str) -> Option<String> {
    let snapshot = app.state::<AppState>().snapshot.lock().unwrap().clone();
    snapshot
        .accounts
        .iter()
        .find(|a| a.provider == provider && a.id == id)
        .and_then(|a| a.alias.clone().or(a.email.clone()))
}

async fn switch_to(app: &AppHandle, provider: Provider, id: String, auto: bool) -> Result<(), String> {
    let found = app.state::<AppState>().sources.lock().unwrap().clone();
    let from = {
        let snapshot = app.state::<AppState>().snapshot.lock().unwrap().clone();
        snapshot.accounts.iter().find(|a| a.provider == provider && a.active).map(|a| a.id.clone())
    };
    let target = id.clone();
    let started = Instant::now();
    let result = tauri::async_runtime::spawn_blocking(move || accounts::switch(&found, provider, &target))
        .await
        .map_err(|e| e.to_string()).and_then(|r| r);
    app.state::<AppState>().diagnostics.record(Some(provider), Some(&id), diagnostics::Event::Action {
        action: if auto { diagnostics::Action::SwitchAuto } else { diagnostics::Action::SwitchManual },
        code: diagnostics::Code::from_error(result.as_ref().err().map(String::as_str)), elapsed_ms: diagnostics::millis(started) });
    result?;
    // The account left behind gets its project profile back at once, not at the next refresh.
    sync_pins(app, None).await;
    let event = Switched {
        provider,
        from: from.and_then(|f| account_label(app, provider, &f)),
        to: account_label(app, provider, &id),
        auto,
    };
    crate::alerts::on_switch(app, event.provider, event.to.as_deref(), event.auto);
    let _ = app.emit("account-switched", event);
    Ok(())
}

#[tauri::command]
async fn switch_account(app: AppHandle, provider: Provider, id: String) -> Result<Snapshot, String> {
    switch_to(&app, provider, id.clone(), false).await?;
    switch_desktop(&app, provider, &id).await;
    Ok(do_refresh(&app).await)
}

/// Opens a terminal running the CLI's own sign-in; the account appears once it finishes.
#[tauri::command]
async fn add_account(app: AppHandle, provider: Provider) -> Result<(), String> {
    start_add_account(app, provider, None).await
}

/// Starts the CLI sign-in; `pending` (held until the sign-in ends, not through the refresh after
/// it) keeps a second one from starting, and carries the sign-in's mark as open for as long (see
/// `AppState::sign_in_open`).
async fn start_add_account(app: AppHandle, provider: Provider, pending: Option<monitoring::Operation>) -> Result<(), String> {
    let found = {
        let known = app.state::<AppState>().sources.lock().unwrap().clone();
        if known.is_empty() {
            tauri::async_runtime::spawn_blocking(sources::discover).await.unwrap_or_default()
        } else {
            known
        }
    };
    // Open from before its page can open until the account is saved or the sign-in ends otherwise.
    let pending = (pending, app.state::<AppState>().sign_ins.open());
    // A fresh profile of its own (who signs in is not known yet), held until the sign-in ends.
    let session = signin_session(&app);
    let options = signin_options(&app, None, session.as_deref());
    // Looking for the CLI runs wsl.exe and reads the registry and disk; keep it off the async runtime.
    let job = tauri::async_runtime::spawn_blocking(move || accounts::start_login_observed(provider, &found, None, options))
        .await
        .map_err(|e| e.to_string())??;
    tauri::async_runtime::spawn(async move {
        let added = job.wait(&http_client()).await;
        drop(session);
        // The provider is free for another sign-in before anything waits for a refresh.
        drop(pending);
        if let Some(slot) = added {
            forget_pending(slot.provider, &slot.id);
            let _ = app.emit("account-added", (slot.provider, slot.email));
            do_refresh(&app).await;
        }
    });
    Ok(())
}

/// A brand-new browser profile for a sign-in, or for an email's run of them, when the user chose
/// those (see `signin::Session`). Dropping the last handle closes its browser and deletes it.
fn signin_session(app: &AppHandle) -> Option<Arc<signin::Session>> {
    let window = app.state::<AppState>().settings.lock().unwrap().signin_window;
    (window == signin::Window::Profile).then(signin::Session::create).flatten().map(Arc::new)
}

/// Where a sign-in as `email` opens its page, from the user's choice (see `signin`): `session`'s
/// profile, a private window, or the CLI's default browser.
fn signin_options(app: &AppHandle, email: Option<&str>, session: Option<&signin::Session>) -> accounts::LoginOptions {
    let window = app.state::<AppState>().settings.lock().unwrap().signin_window;
    accounts::LoginOptions { email: email.map(str::to_owned), target: signin::Target::choose(window, session) }
}

/// A sign-in another computer completes in its own browser (see `relay_start`): its page is handed
/// over under `nonce`, and nothing opens here.
fn relay_options(email: Option<&str>, nonce: &str) -> accounts::LoginOptions {
    accounts::LoginOptions { email: email.map(str::to_owned), target: Some(signin::Target::Relay(nonce.to_owned())) }
}

/// A restored account that is signed in again is no longer pending.
fn forget_pending(provider: Provider, id: &str) {
    let mut list = bulk::load_pending();
    let before = list.len();
    list.retain(|p| !(p.provider == provider && p.id == id));
    if list.len() != before {
        let _ = bulk::save_pending(&list);
    }
}

/// Signs a restored account in again, or makes a new CLI sign-in for an account whose saved one
/// ended or ends soon (`bulk::Kind::Renew`), as "Add account" does, but only as that account: a
/// login of any other account is not saved and is revoked, the way monitoring rejects a wrong
/// account. The new login replaces the vault's saved one; nothing is signed out, and the CLI's own
/// files are never written. Returns once the user is done: whether a login was saved (a renewal is
/// not, once the account is in use or was removed meanwhile; see `accounts::store_renewal`). With
/// `relay` (a nonce), another computer completes it in its own browser (see `relay_start`).
async fn add_expected(app: &AppHandle, expected: &bulk::Step, session: Option<&signin::Session>, relay: Option<&str>) -> Result<bool, String> {
    let provider = expected.target.provider;
    let id = expected.target.id.clone();
    let _operation = app.state::<AppState>().account_operations.begin(provider, "add-account")?;
    // The account itself too, so it is not removed (`remove_one`) while its sign-in is open.
    let _account = app.state::<AppState>().account_operations.begin(provider, &id)?;
    let _sign_in = app.state::<AppState>().account_operations.begin_key(monitoring::SIGN_IN)?;
    let _open = app.state::<AppState>().sign_ins.open();
    let found = app.state::<AppState>().sources.lock().unwrap().clone();
    let options = match relay {
        Some(nonce) => relay_options(expected.email.as_deref(), nonce),
        None => signin_options(app, expected.email.as_deref(), session),
    };
    let job = tauri::async_runtime::spawn_blocking(move || accounts::start_login_observed(provider, &found, None, options))
        .await.map_err(|_| "monitor_login_failed".to_string())??;
    let client = http_client();
    let login = job.wait_isolated(&client).await.map_err(String::from)?;
    // The slot id names the account and, for Claude, its organization (server-verified above).
    if login.provider != provider || login.id != id {
        // Never a token a stored login or monitor holds; this fresh login holds none of those.
        app.state::<AppState>().monitor.discard_unstored_login(&login, &accounts::load_slots(), &client).await;
        return Err(wrong_account(provider, expected.email.as_deref(), login.email.as_deref()));
    }
    if expected.kind == bulk::Kind::Renew {
        // Not if the CLI was switched to the account meanwhile, or the account removed.
        let (renewal, sources) = (login.clone(), app.state::<AppState>().sources.lock().unwrap().clone());
        let renewed = tauri::async_runtime::spawn_blocking(move || accounts::store_renewal(&renewal, &sources)).await
            .map_err(|_| "monitor_storage".to_string())?.map_err(|_| "monitor_storage".to_string())?;
        if renewed != accounts::Renewed::Saved {
            return Ok(false);
        }
    } else {
        accounts::store_login(&login).map_err(|_| "monitor_storage".to_string())?;
        forget_pending(provider, &login.id);
        let _ = app.emit("account-added", (login.provider, login.email.clone()));
    }
    refresh_soon(app);
    Ok(true)
}

/// A refresh after a sign-in, not waited for: one can take most of a minute (finding sign-ins,
/// sharing), and "Sign in all accounts" opens its next sign-in meanwhile. Asked for again while
/// one waits to start, it is that one. Also how a refresh that gave up waiting is made up for
/// (`refresh_within`). Its task waits for the refresh lock without a limit, but holds nothing
/// meanwhile and nobody waits for it, so a lock held for good keeps this one task waiting, and
/// nothing else.
fn refresh_soon(app: &AppHandle) {
    static WAITING: refresh_wait::Coalesce = refresh_wait::Coalesce::new();
    if !WAITING.ask() {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // After any refresh running now, which may have read before the change.
        drop(app.state::<AppState>().refresh_lock.lock().await);
        WAITING.started();
        do_refresh(&app).await;
    });
}

/// Lets the account's next fetch skip an earlier error's backoff (`providers::retry_now`). The next
/// refresh does it under the refresh lock, so a refresh running now cannot write the old backoff
/// back; nobody waits for the lock to ask (see `refresh_wait::CacheDue`).
fn retry_soon(app: &AppHandle, key: String) {
    app.state::<AppState>().cache_due.retry(key);
}

/// Why a sign-in was not the expected account. A Codex account is one ChatGPT workspace, so the
/// right email with another id means the person picked another workspace on the page.
fn wrong_account(provider: Provider, expected: Option<&str>, signed_in: Option<&str>) -> String {
    let same_email = matches!((expected, signed_in), (Some(a), Some(b)) if a.trim().eq_ignore_ascii_case(b.trim()));
    if provider == Provider::Codex && same_email { "monitor_wrong_workspace" } else { "monitor_wrong_account" }.into()
}

/// What another of the user's computers asks before completing this one's sign-ins in its own
/// browser (see `relay::ask`): who this computer is, the plan its "Sign in all accounts" would run
/// now, which accounts it reads usage for itself (automatic switching per provider, rotation
/// thresholds), its accounts in use whose CLI sign-ins ended or end soon (the CLI manages those: no
/// sitting renews them), whether a sign-in is open here or its own run goes, and what its OpenSSH
/// server starts. A relayed sign-in of `from`'s own still open here (its stop on the way) is not
/// busy: its next start replaces it (see `relay_start`).
async fn relay_plan(app: &AppHandle, monitoring: bool, from: &str) -> serde_json::Value {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let snapshot = state.snapshot.lock().unwrap().clone();
    let running = state.bulk_connect.lock().unwrap().as_ref().is_some_and(|b| !b.finished);
    let its_own = !from.is_empty() && relay::SLOT.holder().is_some_and(|(_, holder)| holder == from);
    let busy = running || (!its_own && (state.account_operations.busy(monitoring::SIGN_IN) || relay::SLOT.busy()));
    let ssh_shell = tauri::async_runtime::spawn_blocking(pairing::ssh_shell).await.ok().flatten();
    serde_json::json!({
        "machine": presence::identity().map(|(id, name)| serde_json::json!({ "id": id, "name": name })),
        "relay": relay::VERSION,
        "plan": sign_in_plan(app, monitoring),
        "own_readings": { "claude": settings.balancer.claude.auto, "codex": settings.balancer.codex.auto, "rotate": settings.rotate_at.keys().collect::<Vec<_>>() },
        "in_use": in_use_ending(&snapshot, chrono::Utc::now()),
        "busy": busy,
        "ssh_shell": ssh_shell,
    })
}

/// This computer's accounts in use whose saved CLI sign-in ended or ends within
/// `monitoring::RENEW_DAYS`: the CLI manages that sign-in, so nothing here renews it, and a sitting
/// names it (`claude /login` at this computer).
fn in_use_ending(snapshot: &Snapshot, now: chrono::DateTime<chrono::Utc>) -> Vec<relay::InUse> {
    let horizon = now + chrono::Duration::days(monitoring::RENEW_DAYS);
    snapshot.accounts.iter().filter(|a| a.active && !a.id.starts_with("token:") && a.cli_ends_at.is_some_and(|e| e <= horizon))
        .map(|a| relay::InUse { machine: String::new(), provider: a.provider, id: a.id.clone(), email: a.email.clone(), ends_at: a.cli_ends_at })
        .collect()
}

/// Another of the user's computers (`from`) completing one of this computer's sign-ins in its own
/// browser ("Sign in all your computers from here", see `relay`): starts sign-in `kind` for the
/// account under that computer's id `step`, with no window or browser here, and answers with its
/// state once its page is ready or it ended (waiting up to 40 seconds). Only a sign-in this
/// computer's own "Sign in all accounts" would make now, checked as a run checks a step at its turn
/// (whether the account is saved, from the vault itself, so a monitoring sign-in right after its
/// account was added is not missed); never while this computer's own run goes, a sign-in is open
/// here or another computer's relayed one is. A step of `from`'s still open (its stop never came)
/// gives way. `machine` must be this computer's id. This computer's desktop need not be shown:
/// nothing opens on it. The sign-in stops when that computer asks, stops asking after it, or the
/// CLI never hands over its page (see `relay::Slot::should_stop`).
#[allow(clippy::too_many_arguments)]
async fn relay_start(app: &AppHandle, step: String, from: String, machine: String, kind: bulk::Kind, provider: Provider, id: String, email: Option<String>) -> Result<serde_json::Value, String> {
    let to_value = |state: relay::State| serde_json::to_value(state).map_err(|e| e.to_string());
    if presence::identity().is_none_or(|(id, _)| id.as_str() != machine) {
        return Err("wrong_machine".into());
    }
    // The same step again (a start whose answer was lost): how it goes, nothing new.
    if let Some(state) = relay::SLOT.state(&step) {
        return to_value(relay::SLOT.wait_page(&step, Duration::from_secs(40)).await.unwrap_or(state));
    }
    // A step of the same computer's that is still open (its stop did not arrive) gives way.
    if let Some((old, old_from)) = relay::SLOT.holder() {
        if old_from == from {
            relay::SLOT.stop(&old);
            let until = Instant::now() + Duration::from_secs(5);
            while relay::SLOT.busy() && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    let state = app.state::<AppState>();
    let running = state.bulk_connect.lock().unwrap().as_ref().is_some_and(|b| !b.finished);
    if running || state.account_operations.busy(monitoring::SIGN_IN) || relay::SLOT.busy() {
        return Err("signin_busy".into());
    }
    let target = bulk::Target { provider, id: id.clone(), account: email.clone(), ended: None };
    let snapshot = state.snapshot.lock().unwrap().clone();
    let saved = tauri::async_runtime::spawn_blocking({
        let id = id.clone();
        move || accounts::load_slots().iter().any(|s| s.provider == provider && s.id == id)
    }).await.unwrap_or(false);
    let active = snapshot.accounts.iter().any(|a| a.provider == provider && a.id == id && a.active);
    let due = match kind {
        bulk::Kind::Add => !saved && pending_accounts(&state, &snapshot).iter().any(|p| p.provider == provider && p.id == id),
        bulk::Kind::Renew => saved && !active && cli_sign_in_due(app, &target),
        bulk::Kind::Connect => saved && needs_monitor_sign_in(app, &target).await,
    };
    if !due {
        return to_value(relay::State { step, phase: relay::Phase::Skipped, url: None, error: None, revision: relay::SLOT.revision() });
    }
    let nonce = match relay::SLOT.begin(&step, &from, provider, email.as_deref())? {
        relay::Begun::New(nonce) => nonce,
        relay::Begun::Again => return relay::SLOT.state(&step).map_or_else(|| Err("relay_step_unknown".into()), to_value),
    };
    let h = app.clone();
    let task_step = step.clone();
    tauri::async_runtime::spawn(async move {
        let step = task_step;
        // However this task ends, the slot is never left held.
        let _end = relay::EndGuard { slot: &relay::SLOT, step: step.clone() };
        let expected = bulk::Step { kind, target, email, machine: None };
        let run = async {
            match kind {
                bulk::Kind::Add | bulk::Kind::Renew => add_expected(&h, &expected, None, Some(&nonce)).await,
                bulk::Kind::Connect => {
                    // Asked for by the user, at the other computer: a manual start, never an automatic one.
                    let key = format!("{}:{}", provider.name(), id);
                    auto_monitor_saved(&h, |t| t.manual(&key, true));
                    match start_monitoring_login(h.clone(), provider, id.clone(), false, None, Some(nonce.clone())).await {
                        // Stopping drops this future, and with it the sign-in.
                        Ok(task) => AbortOnDrop(task).await.unwrap_or_else(|_| Err("monitor_login_failed".into())).map(|()| true),
                        Err(e) => Err(e),
                    }
                }
            }
        };
        let stop = async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Some(why) = relay::SLOT.should_stop(&step) {
                    break why;
                }
            }
        };
        let outcome = tokio::select! {
            outcome = run => outcome,
            why = stop => Err(why.to_string()),
        };
        let (phase, error) = match outcome {
            Ok(true) => (relay::Phase::Done, None),
            // A renewal for an account switched to or removed meanwhile: nothing was saved.
            Ok(false) => (relay::Phase::Skipped, None),
            Err(e) => (relay::Phase::Failed, Some(e)),
        };
        relay::SLOT.finish(&step, phase, error);
    });
    relay::SLOT.wait_page(&step, Duration::from_secs(40)).await.map_or_else(|| Err("relay_step_unknown".into()), to_value)
}

/// A one-time login owns its own refresh-token chain; it never changes the active CLI.
#[tauri::command]
async fn connect_monitoring(app: AppHandle, provider: Provider, id: String) -> Result<(), String> {
    let key = format!("{}:{id}", provider.name());
    auto_monitor_saved(&app, |t| t.manual(&key, true));
    start_monitoring_login(app, provider, id, false, None, None).await.map(|_| ())
}

/// Connect monitoring's sign-in, started by the user or on its own (`auto`, see `auto_monitor`).
/// One sign-in is open at a time across the app (`monitoring::SIGN_IN`); the returned task
/// ends with its outcome once the user is done. With `relay` (a nonce), another computer completes
/// it in its own browser and no profile opens here (see `relay_start`).
async fn start_monitoring_login(app: AppHandle, provider: Provider, id: String, auto: bool, session: Option<Arc<signin::Session>>, relay: Option<String>) -> Result<tauri::async_runtime::JoinHandle<Result<(), String>>, String> {
    let operation = app.state::<AppState>().account_operations.begin(provider, &id)?;
    let sign_in = app.state::<AppState>().account_operations.begin_key(monitoring::SIGN_IN)?;
    let open = app.state::<AppState>().sign_ins.open();
    let expected = accounts::load_slots().into_iter().find(|s| s.provider == provider && s.id == id)
        .ok_or("monitor_wrong_account")?;
    let key = monitoring::key(&expected);
    let started = Instant::now();
    app.state::<AppState>().diagnostics.record(Some(provider), Some(&id), diagnostics::Event::Action { action: diagnostics::Action::ConnectStarted, code: diagnostics::Code::Ok, elapsed_ms: 0 });
    let found = app.state::<AppState>().sources.lock().unwrap().clone();
    let context = diagnostics::Context { recorder: app.state::<AppState>().diagnostics.clone(), provider, id: id.clone() };
    // The email group's profile in "Sign in all accounts", else a fresh one of its own; none for a
    // page another computer opens.
    let session = if relay.is_some() { None } else { session.or_else(|| signin_session(&app)) };
    let options = match relay.as_deref() {
        Some(nonce) => relay_options(expected.email.as_deref(), nonce),
        None => signin_options(&app, expected.email.as_deref(), session.as_deref()),
    };
    let job = tauri::async_runtime::spawn_blocking(move || accounts::start_login_observed(provider, &found, Some(context), options)).await;
    let job = match job {
        Ok(Ok(job)) => job,
        failed => {
            app.state::<AppState>().diagnostics.record(Some(provider), Some(&id), diagnostics::Event::Action { action: diagnostics::Action::ConnectFinished, code: diagnostics::Code::LoginFailed, elapsed_ms: diagnostics::millis(started) });
            // No CLI to sign in with is said so (with how to install one): connecting again would
            // not help.
            return Err(match failed {
                Ok(Err(e)) if e == "CLI not found" => e,
                _ => "monitor_login_failed".into(),
            });
        }
    };
    if auto {
        let name = app.state::<AppState>().snapshot.lock().unwrap().accounts.iter()
            .find(|a| a.provider == provider && a.id == id).and_then(|a| a.alias.clone().or_else(|| a.email.clone()));
        crate::alerts::on_monitor_auto_started(&app, name.as_deref());
        let _ = app.emit("monitoring-auto-started", serde_json::json!({ "provider": provider, "id": id, "name": name }));
    }
    Ok(tauri::async_runtime::spawn(async move {
        let operation = operation;
        let sign_in = sign_in;
        let open = open;
        let outcome = match job.wait_isolated(&http_client()).await {
            Ok(login) => {
                let slots = accounts::load_slots();
                if slots.iter().any(|s| s.provider == provider && s.id == id) {
                    app.state::<AppState>().monitor.connect_replacing(&expected, &login, &slots, &http_client()).await
                        .map_err(|e| if e == "monitor_wrong_account" { wrong_account(provider, expected.email.as_deref(), login.email.as_deref()) } else { e })
                } else {
                    let mut known = slots;
                    known.push(expected.clone());
                    app.state::<AppState>().monitor.discard_unstored_login(&login, &known, &http_client()).await;
                    Err("monitor_wrong_account".into())
                }
            }
            Err(e) => Err(e.into()),
        };
        drop(session);
        drop(operation);
        drop(sign_in);
        drop(open);
        if auto {
            let ok = outcome.is_ok();
            auto_monitor_saved(&app, |t| t.finished(&key, ok, chrono::Utc::now()));
        }
        app.state::<AppState>().diagnostics.record(Some(provider), Some(&id), diagnostics::Event::Action { action: diagnostics::Action::ConnectFinished,
            code: diagnostics::Code::from_error(outcome.as_ref().err().map(String::as_str)), elapsed_ms: diagnostics::millis(started) });
        let _ = app.emit("monitoring-login", serde_json::json!({ "provider": provider, "id": id, "error": outcome.as_ref().err() }));
        // Preserve last-known data, but allow the new login to bypass an earlier error's backoff,
        // without holding up whatever waits for this sign-in (see `retry_soon`, `refresh_soon`).
        retry_soon(&app, key);
        refresh_soon(&app);
        outcome
    }))
}

/// Whether an account needs a monitoring sign-in, by what the monitor holds for it: none of its
/// own, or one that ended, was refused or ends soon (`monitoring::renewal`). A sign-in that just
/// finished counts at once, where the snapshot shows it only after the next refresh.
async fn needs_monitor_sign_in(app: &AppHandle, target: &bulk::Target) -> bool {
    let held = app.state::<AppState>().monitor.health(&[(target.provider, target.id.clone())]).await.into_values().next();
    held.is_none_or(|s| monitoring::renewal(Some(&s), None, chrono::Utc::now()).due())
}

/// Whether an account's saved CLI sign-in still needs a new one (`accounts::cli_renewal`, or the
/// provider rejected it), read from the vault itself, where a sign-in that finished shows at once,
/// before any refresh does. Not once the account is in use: the CLI manages that sign-in.
fn cli_sign_in_due(app: &AppHandle, target: &bulk::Target) -> bool {
    let active = app.state::<AppState>().snapshot.lock().unwrap().accounts.iter()
        .any(|a| a.provider == target.provider && a.id == target.id && a.active);
    !active && accounts::load_slots().into_iter().find(|s| s.provider == target.provider && s.id == target.id)
        .is_some_and(|s| accounts::cli_renewal(s.ends_at(), false, chrono::Utc::now()).is_some()
            || (s.provider == Provider::Claude && providers::cli_rejected(&s)))
}

/// Updates "Connect monitoring for all" and tells Settings.
fn publish_bulk(app: &AppHandle, change: impl FnOnce(&mut BulkConnect)) {
    let state = app.state::<AppState>();
    let mut running = state.bulk_connect.lock().unwrap();
    let mut run = None;
    if let Some(bulk) = running.as_mut() {
        change(bulk);
        bulk.revision = state.bulk_revision.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let _ = app.emit("bulk-monitoring", bulk.clone());
        run = bulk.by_email.then(|| roster_run(bulk, chrono::Utc::now()));
    }
    drop(running);
    state.bulk_changed.notify_waiters();
    // The user's other computers follow "Sign in all accounts" through the roster.
    if let Some(run) = run {
        *state.roster_run.lock().unwrap() = Some(run);
        let settings = state.settings.lock().unwrap().clone();
        if settings.account_roster {
            let h = app.clone();
            tauri::async_runtime::spawn(async move {
                let accounts = h.state::<AppState>().snapshot.lock().unwrap().accounts.clone();
                sync_roster(&h, &settings, accounts, false, false).await;
            });
        }
    }
}

/// A run of "Sign in all accounts" as the roster carries it (see `roster::Run`).
fn roster_run(b: &BulkConnect, at: chrono::DateTime<chrono::Utc>) -> roster::Run {
    let word = |v: serde_json::Value| v.as_str().unwrap_or_default().to_string();
    roster::Run {
        started_at: Some(b.run_started_at),
        open: b.steps.iter().position(|s| s.state == StepState::Running),
        revision: b.revision, at, total: b.total, done: b.done, emails: b.emails, email: b.email, finished: b.finished,
        still_waiting: b.still_waiting, monitoring: b.monitoring, error: b.error.clone(), everywhere: b.everywhere,
        steps: b.steps.iter().map(|s| roster::RunStep {
            provider: s.target.provider, account: s.target.account.clone(),
            kind: word(serde_json::json!(s.kind)), state: word(serde_json::json!(s.state)), error: s.error.clone(),
        }).collect(),
    }
}

/// Returns once the run has changed since `revision` (see `BulkConnect::revision`), or after
/// `wait`: an agent elsewhere follows a run without polling.
async fn wait_for_sign_in_change(app: &AppHandle, revision: u64, wait: Duration) {
    let state = app.state::<AppState>();
    let until = tokio::time::Instant::now() + wait;
    loop {
        let changed = state.bulk_changed.notified();
        tokio::pin!(changed);
        // Listening before looking, so a change between the two is never missed.
        changed.as_mut().enable();
        if state.bulk_revision.load(std::sync::atomic::Ordering::SeqCst) > revision {
            return;
        }
        if tokio::time::timeout_at(until, changed).await.is_err() {
            return;
        }
    }
}

/// Queues a monitoring sign-in for every account that needs one (`bulk::needs_sign_in`: none yet,
/// or one ended, refused or ending soon; see `run_sign_ins`). Returns the queued accounts.
async fn connect_all(app: &AppHandle) -> Result<Vec<bulk::Target>, String> {
    let groups = bulk::connect_plan(&app.state::<AppState>().snapshot.lock().unwrap());
    let targets = groups.iter().flat_map(|g| g.steps.iter().map(|s| s.target.clone())).collect();
    run_sign_ins(app, groups, false, true, None)?;
    Ok(targets)
}

/// "Sign in all accounts": signs every pending account in again and, with `monitoring`, connects
/// monitoring for each account that needs it, one email at a time (see `bulk::sign_in_plan`).
/// Returns the plan.
async fn sign_in_all(app: &AppHandle, monitoring: bool) -> Result<Vec<bulk::Group>, String> {
    let groups = sign_in_plan(app, monitoring);
    run_sign_ins(app, groups.clone(), true, monitoring, None)?;
    Ok(groups)
}

/// Retry (the stop and "still waiting" notifications, Settings > Accounts): stops the run if one
/// is going, then starts the same kind again. Its plan is made afresh from what is still missing,
/// so what already worked is not asked for again.
async fn retry_run(app: &AppHandle) -> Result<(), String> {
    let last = app.state::<AppState>().bulk_connect.lock().unwrap().clone();
    stop_and_wait(app).await?;
    match last {
        Some(b) if !b.by_email => connect_all(app).await.map(|_| ()),
        // Every computer again, those left out last time too, through the person's confirmation
        // in Settings (what already worked is not asked for).
        Some(b) if b.everywhere => {
            confirm_everywhere_here(app, b.monitoring);
            Ok(())
        }
        Some(b) => sign_in_all(app, b.monitoring).await.map(|_| ()),
        None => sign_in_all(app, true).await.map(|_| ()),
    }
}

fn sign_in_plan(app: &AppHandle, monitoring: bool) -> Vec<bulk::Group> {
    let state = app.state::<AppState>();
    let snapshot = state.snapshot.lock().unwrap().clone();
    bulk::sign_in_plan(&snapshot, &pending_accounts(&state, &snapshot), monitoring)
}

/// Accounts to sign in here: those a restored backup named, and those other machines' rosters
/// name (see `roster::waiting`).
fn pending_accounts(state: &AppState, snapshot: &Snapshot) -> Vec<bulk::Pending> {
    let mut list = bulk::load_pending();
    bulk::add_pending(&mut list, roster_waiting(state, snapshot));
    list
}

/// The accounts other machines' rosters name that are not signed in here, while the roster is on.
fn roster_waiting(state: &AppState, snapshot: &Snapshot) -> Vec<bulk::Pending> {
    if !state.settings.lock().unwrap().account_roster {
        return vec![];
    }
    roster::waiting(&state.roster_seen.lock().unwrap(), &snapshot.accounts)
}

/// A task that is aborted when this is dropped, as a stopped "Sign in all accounts" drops its
/// open sign-in.
struct AbortOnDrop<T>(tauri::async_runtime::JoinHandle<T>);

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = tauri::Result<T>;
    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.0).poll(cx)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Stops the run if one is going and waits for it to end.
async fn stop_and_wait(app: &AppHandle) -> Result<(), String> {
    let going = || app.state::<AppState>().bulk_connect.lock().unwrap().as_ref().is_some_and(|b| !b.finished);
    if going() {
        stop_running_sign_ins(&app.state::<AppState>());
        let until = Instant::now() + Duration::from_secs(20);
        while going() {
            if Instant::now() > until {
                return Err("monitor_busy".into());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    Ok(())
}

/// Resolves once `session`'s browser, having opened for any of the email's sign-ins, is closed:
/// the user closed the window, so the sign-in can no longer finish. Never resolves without one.
async fn browser_closed(session: Option<&signin::Session>) {
    let Some(session) = session else { return std::future::pending().await };
    while !session.browser_closed() {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Resolves once a sign-in page opened in `session`'s profile at or after `since`; never without one.
async fn page_opened(session: Option<&signin::Session>, since: std::time::SystemTime) {
    let Some(session) = session else { return std::future::pending().await };
    while !session.page_opened_since(since) {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// What a sign-in is, as an email's page lists it: "Claude" (the account's own sign-in, new or
/// made again), "Codex monitoring", "Claude for VIRTUAL-TEN" (another computer's, see `relay`).
fn step_name(lang: &str, step: &bulk::Step) -> String {
    let provider = match step.target.provider { Provider::Claude => "Claude", Provider::Codex => "Codex" };
    let key = if step.kind == bulk::Kind::Connect { "signinPageStepConnect" } else { "signinPageStepAdd" };
    let name = crate::i18n::t(lang, key, &[("provider", provider)]);
    // Another computer's sign-in names that computer: one email's page lists every computer's.
    match &step.machine {
        Some(machine) => for_machine(lang, &name, &machine.name),
        None => name,
    }
}

/// How long a sign-in may take to open its page (Codex's app server can be a cold start).
const NO_PAGE_LIMIT: Duration = Duration::from_secs(40);
/// How long a sign-in may wait on the person before they are asked whether to retry it.
const STILL_WAITING: Duration = Duration::from_secs(5 * 60);

/// Watches the open sign-in (`turn`'s): resolves with `signin_no_page` when its page never opened
/// in the email's profile (only a profile can tell; a page refused for its port, whose CLI starts
/// again, counts the wait from then), and, once it has waited long, asks the person at the desktop
/// whether to start it again (and never resolves after that).
async fn watch_step(app: &AppHandle, session: Option<&signin::Session>, since: std::time::SystemTime, browser: bool, turn: &Turn<'_>) -> String {
    let started = tokio::time::Instant::now();
    if let Some(session) = session {
        let mut waiting_from = started;
        let mut refused = None;
        loop {
            tokio::time::sleep_until(waiting_from + NO_PAGE_LIMIT).await;
            if session.page_opened_since(since) {
                break;
            }
            let last = session.page_refused_since(since);
            if last.is_none() || last == refused {
                return "signin_no_page".into();
            }
            refused = last;
            waiting_from = tokio::time::Instant::now();
        }
    }
    tokio::time::sleep_until(started + STILL_WAITING).await;
    publish_bulk(app, |b| b.still_waiting = true);
    // Another computer's sign-in waits in a browser here, whatever the window setting says.
    let place = browser.then(|| crate::i18n::t(&crate::i18n::resolve(&app.state::<AppState>().settings.lock().unwrap().language), "signInWhereBrowser", &[]));
    let mut vars = vec![("account", turn.account)];
    if let Some(place) = place.as_deref() {
        vars.insert(0, ("where", place));
    }
    still_waiting_choice(app, &vars, turn.restart, turn.at);
    std::future::pending().await
}

/// Runs the sign-ins one terminal at a time, each started once the previous one finished, an
/// email's sign-ins back to back in one fresh browser profile so Google is asked once for all of
/// them; the profile's browser is closed and the profile deleted when the email's turn ends.
/// Stops at the first that fails (the user closed it, closed the browser, signed in as another
/// account, its page never opened, or Stop was pressed). Every step's outcome stays in `steps`.
fn run_sign_ins(app: &AppHandle, groups: Vec<bulk::Group>, by_email: bool, monitoring: bool, gathered: Option<Gathered>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let total = groups.iter().map(|g| g.steps.len()).sum();
    let stop = Arc::new(tokio::sync::Notify::new());
    let restart = Arc::new(StepRestart::default());
    {
        let mut running = state.bulk_connect.lock().unwrap();
        if running.as_ref().is_some_and(|b| !b.finished) {
            return Err("monitor_busy".into());
        }
        let steps = groups.iter().flat_map(|g| g.steps.iter())
            .map(|s| StepRecord { target: s.target.clone(), kind: s.kind, machine: s.machine.as_ref().map(|m| m.name.clone()), state: StepState::Pending,
                error: None, started_at: None, page_at: None, ended_at: None }).collect();
        let (left_out, in_use) = gathered.clone().map(|g| (g.left_out, g.in_use)).unwrap_or_default();
        *running = Some(BulkConnect { run_started_at: chrono::SubsecRound::trunc_subsecs(chrono::Utc::now(), 3), total, done: 0, current: None, kind: None, emails: if by_email { groups.len() } else { 0 }, email: 0,
            finished: total == 0, error: None, steps, by_email, monitoring, still_waiting: false, revision: 0,
            everywhere: gathered.is_some(), left_out, in_use });
        *state.bulk_stop.lock().unwrap() = Some(stop.clone());
        *state.bulk_restart.lock().unwrap() = Some(restart.clone());
    }
    publish_bulk(app, |_| {});
    let h = app.clone();
    tauri::async_runtime::spawn(async move {
        let lang = crate::i18n::resolve(&h.state::<AppState>().settings.lock().unwrap().language);
        let mut error = None;
        // Whose sign-in failed, for a message that names its CLI (`cliMissing`).
        let mut failed_provider = None;
        let mut done = 0;
        let mut position = 0;
        let mark = |at: usize, state: StepState, error: Option<String>| publish_bulk(&h, |b| {
            if let Some(step) = b.steps.get_mut(at) {
                let now = Some(chrono::Utc::now());
                match state {
                    StepState::Running => step.started_at = now,
                    StepState::Pending => {}
                    _ => step.ended_at = now,
                }
                step.state = state;
                step.error = error;
            }
            // A sign-in that ended waits on nobody, even before the next one starts.
            if state != StepState::Running {
                b.still_waiting = false;
            }
        });
        // Accounts this run signed in: one only just added has its monitoring sign-in next, though
        // the refresh that lists it may still be running.
        let mut added = std::collections::HashSet::new();
        // Other computers left out as the run went (their remaining sign-ins too), and how many of
        // each one's sign-ins failed on their own (see `relay::fault`).
        let mut dropped: HashMap<String, String> = HashMap::new();
        let mut strikes: HashMap<String, u32> = HashMap::new();
        'groups: for (index, group) in groups.iter().enumerate() {
            // Made at the email's first sign-in that still has to run, ended with its turn.
            let mut group_session: Option<Option<Arc<signin::Session>>> = None;
            // What the email's page shows of its turn (see `signin::Session::show`).
            let mut page = signin::Progress {
                steps: group.steps.iter().map(|s| (step_name(&lang, s), signin::PageStep::Pending)).collect(),
                at: 0,
                total,
            };
            for (in_group, step) in group.steps.iter().enumerate() {
                let target = &step.target;
                let at = position;
                position += 1;
                publish_bulk(&h, |b| {
                    b.done = done;
                    b.current = Some(target.clone());
                    b.kind = by_email.then_some(step.kind);
                    b.email = if by_email { index + 1 } else { 0 };
                    b.still_waiting = false;
                });
                done += 1;
                // Another computer's sign-in, completed here (see `relay`): that computer judges
                // whether it is still needed, so none of the checks below applies to it.
                if let Some(machine) = &step.machine {
                    if let Some(reason) = dropped.get(&machine.id) {
                        mark(at, StepState::Skipped, Some(reason.clone()));
                        page.steps[in_group].1 = signin::PageStep::Left;
                        continue;
                    }
                    // The page opens here: this computer's screen must be seen.
                    if let Err(problem) = desktop_shown() {
                        done -= 1;
                        mark(at, StepState::Failed, Some(problem.clone()));
                        error = Some(problem);
                        break 'groups;
                    }
                    mark(at, StepState::Running, None);
                    let free = async {
                        while h.state::<AppState>().account_operations.busy(monitoring::SIGN_IN) {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                    };
                    let stopped = tokio::select! {
                        _ = free => false,
                        _ = stop.notified() => true,
                    };
                    let session = relay_session(&h, &mut group_session, group.email.as_deref(), &lang);
                    page.at = done;
                    page.steps[in_group].1 = signin::PageStep::Opening;
                    if let Some(s) = session.as_deref() {
                        s.show(&page);
                    }
                    let account = for_machine(&lang, &target.account.clone().unwrap_or_default(), &machine.name);
                    // Once per email: its page here lists every computer's sign-in as it goes. Always in a
                    // browser here (never the terminal a sign-in here may open).
                    if by_email && !stopped && in_group == 0 {
                        let (g, e, i, n) = ((index + 1).to_string(), groups.len().to_string(), done.to_string(), total.to_string());
                        let key = if step.kind == bulk::Kind::Connect { "signInAllConnecting" } else { "signInAllAdding" };
                        let browser = crate::i18n::t(&lang, "signInWhereBrowser", &[]);
                        sign_in_notice(&h, key, &[("where", &browser), ("g", &g), ("e", &e), ("i", &i), ("n", &n), ("account", &account)]);
                    }
                    let outcome = if stopped {
                        Err("signin_stopped".to_string())
                    } else {
                        // Its page opened here (true), or it is starting again for a new one (false).
                        let on_page = |open: bool| {
                            publish_bulk(&h, |b| if let Some(s) = b.steps.get_mut(at) { s.page_at = open.then(chrono::Utc::now) });
                            page.steps[in_group].1 = if open { signin::PageStep::Open } else { signin::PageStep::Opening };
                            if let Some(s) = session.as_deref() {
                                s.show(&page);
                            }
                        };
                        let turn = Turn { stop: &stop, restart: &restart, at, account: &account };
                        relayed_turn(&h, step, machine, session.as_ref(), &turn, on_page).await
                    };
                    let left = |page: &mut signin::Progress| {
                        page.steps[in_group].1 = signin::PageStep::Left;
                        if let Some(s) = session.as_deref() {
                            s.show(page);
                        }
                    };
                    match outcome {
                        Ok(true) => {
                            mark(at, StepState::Done, None);
                            page.steps[in_group].1 = signin::PageStep::Done;
                            if let Some(s) = session.as_deref() {
                                s.show(&page);
                            }
                        }
                        Ok(false) => {
                            mark(at, StepState::Skipped, None);
                            page.steps[in_group].1 = signin::PageStep::Skipped;
                            if let Some(s) = session.as_deref() {
                                s.show(&page);
                            }
                        }
                        Err(e) => match relay::fault(&e) {
                            relay::Fault::StopRun => {
                                done -= 1;
                                mark(at, StepState::Failed, Some(e.clone()));
                                error = Some(e);
                                failed_provider = Some(target.provider);
                                break 'groups;
                            }
                            fault => {
                                // Left out: this sign-in alone, or its computer's from here on (also
                                // once two of its sign-ins failed on their own).
                                mark(at, StepState::Skipped, Some(e.clone()));
                                left(&mut page);
                                let strikes = strikes.entry(machine.id.clone()).or_default();
                                *strikes += u32::from(fault == relay::Fault::SkipStep);
                                let skipped = if fault == relay::Fault::DropComputer || *strikes >= 2 {
                                    dropped.insert(machine.id.clone(), e.clone());
                                    relay::Skipped::computer(&machine.name, &e)
                                } else {
                                    relay::Skipped { account: target.account.clone(), ..relay::Skipped::computer(&machine.name, &e) }
                                };
                                publish_bulk(&h, |b| b.left_out.push(skipped));
                            }
                        },
                    }
                    continue;
                }
                // Done (or removed) since the queue started: nothing to do.
                let still = {
                    let state = h.state::<AppState>();
                    let snapshot = state.snapshot.lock().unwrap();
                    let account = snapshot.accounts.iter().find(|a| a.provider == target.provider && a.id == target.id);
                    match step.kind {
                        bulk::Kind::Add => account.is_none(),
                        // Never once the account is in use: the CLI manages that sign-in. Whether
                        // it still needs one is the vault's to say (see below).
                        bulk::Kind::Renew => account.is_some_and(|a| !a.active),
                        // Still here; whether it still needs the sign-in is the monitor's to say.
                        bulk::Kind::Connect => account.is_some() || added.contains(&(target.provider, &target.id)),
                    }
                };
                let still = still && match step.kind {
                    bulk::Kind::Add => true,
                    bulk::Kind::Renew => cli_sign_in_due(&h, target),
                    bulk::Kind::Connect => needs_monitor_sign_in(&h, target).await,
                };
                if !still {
                    mark(at, StepState::Skipped, None);
                    page.steps[in_group].1 = signin::PageStep::Skipped;
                    continue;
                }
                // Locked or left since the queue started: stop rather than open a page nobody sees.
                if let Err(problem) = desktop_shown() {
                    done -= 1;
                    mark(at, StepState::Failed, Some(problem.clone()));
                    error = Some(problem);
                    break 'groups;
                }
                mark(at, StepState::Running, None);
                // Another sign-in is open (one the user or automatic monitoring started): wait for it.
                let free = async {
                    while h.state::<AppState>().account_operations.busy(monitoring::SIGN_IN) {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                };
                let stopped = tokio::select! {
                    _ = free => false,
                    _ = stop.notified() => true,
                };
                // A sign-in that finished during that wait may have connected this account already:
                // never ask for it twice.
                if !stopped && step.kind == bulk::Kind::Connect && !needs_monitor_sign_in(&h, target).await {
                    mark(at, StepState::Skipped, None);
                    page.steps[in_group].1 = signin::PageStep::Skipped;
                    continue;
                }
                // Likewise a CLI sign-in made meanwhile (this run's or Add account's), which the
                // vault holds at once, or a switch to the account, which the snapshot shows.
                if !stopped && step.kind == bulk::Kind::Renew && !cli_sign_in_due(&h, target) {
                    mark(at, StepState::Skipped, None);
                    page.steps[in_group].1 = signin::PageStep::Skipped;
                    continue;
                }
                let session = group_session.get_or_insert_with(|| {
                    let session = signin_session(&h);
                    if let (Some(s), Some(email)) = (session.as_deref(), group.email.as_deref()) {
                        s.label(email, &lang);
                    }
                    session
                }).clone();
                page.at = done;
                page.steps[in_group].1 = signin::PageStep::Opening;
                let show = |session: Option<&signin::Session>, page: &signin::Progress| if let Some(s) = session { s.show(page) };
                show(session.as_deref(), &page);
                let since = std::time::SystemTime::now();
                // One go at the sign-in. "Retry this sign-in" drops it, and its CLI with it,
                // and makes another.
                let start = || {
                    let (h, session) = (&h, session.clone());
                    async move {
                        match step.kind {
                            // A new CLI sign-in saved in the vault: a pending account's, or one for an
                            // account whose saved sign-in ended or ends soon.
                            bulk::Kind::Add | bulk::Kind::Renew => add_expected(h, step, session.as_deref(), None).await,
                            bulk::Kind::Connect => {
                                // The user asked for these, so they count as manual starts, never automatic ones.
                                let key = format!("{}:{}", target.provider.name(), target.id);
                                auto_monitor_saved(h, |t| t.manual(&key, true));
                                match start_monitoring_login(h.clone(), target.provider, target.id.clone(), false, session.clone(), None).await {
                                    // Stopping drops this future, and with it the sign-in.
                                    Ok(task) => AbortOnDrop(task).await.unwrap_or_else(|_| Err("monitor_login_failed".into())).map(|()| true),
                                    Err(e) => Err(e),
                                }
                            }
                        }
                    }
                };
                let run = start();
                // Closing the browser after the email's previous sign-in finished stops nothing.
                if let Some(s) = session.as_deref() {
                    s.rearm();
                }
                // Whoever sits at this desktop (often brought here by an agent on another
                // computer) is told which account and which page comes next.
                let account = target.account.clone().unwrap_or_default();
                if by_email && !stopped {
                    let (g, e, i, n) = ((index + 1).to_string(), groups.len().to_string(), done.to_string(), total.to_string());
                    let key = if step.kind == bulk::Kind::Connect { "signInAllConnecting" } else { "signInAllAdding" };
                    sign_in_notice(&h, key, &[("g", &g), ("e", &e), ("i", &i), ("n", &n), ("account", &account)]);
                }
                let mut run = Box::pin(run);
                let outcome = if stopped {
                    Err("signin_stopped".into())
                } else {
                    let closed = browser_closed(session.as_deref());
                    tokio::pin!(closed);
                    let turn = Turn { stop: &stop, restart: &restart, at, account: &account };
                    restart.begin();
                    let mut watch = Box::pin(watch_step(&h, session.as_deref(), since, false, &turn));
                    let mut opened = Box::pin(page_opened(session.as_deref(), since));
                    let mut opening = true;
                    loop {
                        tokio::select! {
                            // In this order: a sign-in that finished, or Stop, wins over anything
                            // that came at the same moment (a Retry never undoes a finished one).
                            biased;
                            outcome = &mut run => break outcome,
                            _ = stop.notified() => break Err("signin_stopped".into()),
                            // Closing the window once the provider's page says "done" is fine: the
                            // sign-in may still be saving (Codex's CLI can take a while), so it gets up to
                            // two minutes to finish before the queue stops. It waits on nobody then, so a
                            // Retry from elsewhere hears it is not waiting (see `restartable`).
                            _ = &mut closed => {
                                publish_bulk(&h, |b| b.still_waiting = false);
                                break tokio::time::timeout(SIGN_IN_GRACE, &mut run).await.unwrap_or_else(|_| Err("signin_window_closed".into()));
                            }
                            e = &mut watch => break Err(e),
                            _ = &mut opened, if opening => {
                                opening = false;
                                publish_bulk(&h, |b| if let Some(s) = b.steps.get_mut(at) { s.page_at = Some(chrono::Utc::now()) });
                                page.steps[in_group].1 = signin::PageStep::Open;
                                show(session.as_deref(), &page);
                            }
                            _ = restart.notify.notified() => {
                                if !restart.take(at) {
                                    continue;
                                }
                                // Asked for again: this one ends, its CLI with it, and once it has let go
                                // of this computer's one sign-in and the account (a monitoring sign-in
                                // does so a moment later), a new one opens its page in the same window.
                                drop(std::mem::replace(&mut run, Box::pin(start())));
                                let held = [monitoring::SIGN_IN.to_string(), format!("{}:{}", target.provider.name(), target.id),
                                    format!("{}:add-account", target.provider.name())];
                                let free = async {
                                    while held.iter().any(|key| h.state::<AppState>().account_operations.busy(key)) {
                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                    }
                                };
                                tokio::select! {
                                    _ = free => {}
                                    _ = tokio::time::sleep(Duration::from_secs(15)) => {}
                                    _ = stop.notified() => break Err("signin_stopped".into()),
                                }
                                let since = std::time::SystemTime::now();
                                watch = Box::pin(watch_step(&h, session.as_deref(), since, false, &turn));
                                opened = Box::pin(page_opened(session.as_deref(), since));
                                opening = true;
                                publish_bulk(&h, |b| {
                                    b.still_waiting = false;
                                    if let Some(s) = b.steps.get_mut(at) {
                                        s.page_at = None;
                                    }
                                });
                                page.steps[in_group].1 = signin::PageStep::Opening;
                                show(session.as_deref(), &page);
                                if let Some(s) = session.as_deref() {
                                    s.rearm();
                                }
                                restart.started();
                            }
                        }
                    }
                };
                match outcome {
                    Ok(true) => {
                        mark(at, StepState::Done, None);
                        if step.kind == bulk::Kind::Add {
                            added.insert((target.provider, &target.id));
                        }
                        page.steps[in_group].1 = signin::PageStep::Done;
                        show(session.as_deref(), &page);
                    }
                    // A renewal for an account the CLI was switched to, or that was removed, while
                    // its sign-in was open: nothing to save, and nothing for the rest to wait on.
                    Ok(false) => {
                        mark(at, StepState::Skipped, None);
                        page.steps[in_group].1 = signin::PageStep::Skipped;
                        show(session.as_deref(), &page);
                    }
                    Err(e) => {
                        done -= 1;
                        mark(at, StepState::Failed, Some(e.clone()));
                        error = Some(e);
                        failed_provider = Some(target.provider);
                        drop(run);
                        drop(group_session);
                        drop(session);
                        break 'groups;
                    }
                }
            }
            // The last handle: closes the browser and deletes the profile (see `signin::Session`).
            drop(group_session);
        }
        if by_email && total > 0 {
            match &error {
                Some(e) => {
                    let known = e.starts_with("signin_") || e.starts_with("monitor_") || e.starts_with("desktop_");
                    let reason = match failed_provider {
                        Some(provider) if e == "CLI not found" => {
                            let (cli, install) = crate::cli::install_hint(provider);
                            crate::i18n::t(&lang, "cliMissing", &[("cli", cli), ("install", install)])
                        }
                        _ if known => crate::i18n::t(&lang, e, &[]),
                        _ => e.clone(),
                    };
                    // A sitting counts what it made, and names who it left out.
                    let (made, left) = if gathered.is_some() {
                        let state = h.state::<AppState>();
                        let run = state.bulk_connect.lock().unwrap();
                        let made = run.as_ref().map_or(0, |b| b.steps.iter().filter(|s| s.state == StepState::Done).count());
                        (made, run.as_ref().map(|b| left_out_text(&lang, &b.left_out)).unwrap_or_default())
                    } else {
                        (done, String::new())
                    };
                    let reason = if left.is_empty() { reason } else { format!("{reason} {}", crate::i18n::t(&lang, "signInLeftOut", &[("list", &left)])) };
                    sign_in_choice(&h, "bulkConnectStopped", &[("done", &made.to_string()), ("n", &total.to_string()), ("error", &reason)]);
                }
                None if gathered.is_some() => {
                    // Counted as the person sees it: the sign-ins made, and who was left out.
                    let (made, left) = {
                        let state = h.state::<AppState>();
                        let run = state.bulk_connect.lock().unwrap();
                        let made = run.as_ref().map_or(0, |b| b.steps.iter().filter(|s| s.state == StepState::Done).count());
                        (made, run.as_ref().map(|b| b.left_out.clone()).unwrap_or_default())
                    };
                    let left = left_out_text(&lang, &left);
                    let key = if left.is_empty() { "signInEverywhereDone" } else { "signInEverywhereDoneLeft" };
                    sign_in_notice(&h, key, &[("n", &made.to_string()), ("left", &left)]);
                }
                None => sign_in_notice(&h, "signInAllDone", &[("n", &total.to_string())]),
            }
        }
        publish_bulk(&h, |b| {
            b.done = done;
            b.current = None;
            b.kind = None;
            b.finished = true;
            b.error = error;
            b.still_waiting = false;
        });
    });
    Ok(())
}

/// How long a sign-in may go on finishing after its browser window was closed.
const SIGN_IN_GRACE: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------------------------
// "Sign in all your computers from here" (see `relay`)

/// What gathering every computer's plan found besides the plan itself.
#[derive(Clone, Default)]
struct Gathered {
    left_out: Vec<relay::Skipped>,
    in_use: Vec<relay::InUse>,
}

/// A sitting's plan as gathered (see `gather_everywhere`): kept for 10 minutes so the person's OK
/// runs exactly what was shown.
#[derive(Clone, Serialize)]
struct Everywhere {
    id: String,
    #[serde(skip)]
    at: Instant,
    monitoring: bool,
    /// The sign-ins by email, this computer's first in each; another computer's name theirs.
    groups: Vec<bulk::Group>,
    /// Each computer with sign-ins in it, by machine id: its name, the SSH host that reaches it,
    /// its sign-ins and their emails (see `relay::computers`).
    computers: Vec<relay::Computer>,
    /// This computer's machine id, so the confirmation can mark it.
    here: String,
    left_out: Vec<relay::Skipped>,
    in_use: Vec<relay::InUse>,
}

/// How long a gathered plan may be started.
const EVERYWHERE_FRESH: Duration = Duration::from_secs(600);

/// What `sign_in_all` with everywhere tells the agent next: for a preview, after asking the person
/// here, when the notification could not be shown, and with nothing to sign in. Each fits
/// `mcp::MAX_TEXT`, so it reaches the agent whole.
const EVERYWHERE_PREVIEW: &str = "Nothing was started. confirm: true asks the person here, with a notification, to review it; Settings starts it only \
    on their OK. Then its sign-ins open in this computer's browser one at a time, an email's together in one fresh profile, and nothing opens on \
    the other computers' screens. left_out: computers or sign-ins left out, and why. in_use: accounts in use whose CLI sign-in ends, which no \
    sitting renews (claude /login there). Progress: get_machine_readiness's sign_in.";
const EVERYWHERE_ASKED: &str = "A notification on this computer asks the person here to review it: Settings then checks every computer again and \
    starts only on their OK. Then follow it with get_machine_readiness and its sign_in.";
const EVERYWHERE_UNASKED: &str = "The notification asking the person here could not be shown (notifications off?): they start it themselves in \
    Settings > Accounts, Sign in all your computers from here.";
const EVERYWHERE_NOTHING: &str = "Nothing to sign in on any computer reached; left_out says which were not reached and why.";

/// The `Host` names in the user's ~/.ssh/config, lowercased, without patterns: another computer
/// the roster names but nothing here remembers a host for is tried at its name when one matches.
fn ssh_aliases() -> HashSet<String> {
    let text = dirs::home_dir().and_then(|h| std::fs::read_to_string(h.join(".ssh").join("config")).ok()).unwrap_or_default();
    text.lines().filter_map(|l| {
        let l = l.trim();
        let (word, rest) = l.split_once(char::is_whitespace)?;
        word.eq_ignore_ascii_case("host").then_some(rest)
    }).flat_map(|names| names.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>())
        .filter(|n| !n.contains(['*', '?', '!']) && pairing::valid_host(n)).collect()
}

/// Gathers "Sign in all your computers from here": this computer's "Sign in all accounts" plan and
/// every other computer's (`relay::ask`), merged by email (`bulk::everywhere`). The other computers
/// are those paired or checked from here (at the SSH host remembered for each) and the roster's,
/// tried at their name when ~/.ssh/config has it (remembered once one answers as that machine).
/// Their steps are kept only for accounts this computer knows (its own, or a roster's, with the
/// same email), so another computer can never have pages for someone else's accounts opened here;
/// and their monitoring sign-ins only where they read usage for themselves (automatic switching, a
/// rotation threshold) or this computer's shared readings would not cover them there (it does not
/// read that account, does not share readings, or that computer does not read its roster). Kept as
/// the plan the person's OK starts (see `start_everywhere`).
async fn gather_everywhere(app: &AppHandle, monitoring: bool) -> Result<Everywhere, String> {
    let state = app.state::<AppState>();
    let (here_id, here_name) = presence::identity().map(|(id, name)| (id.to_string(), name.to_string())).ok_or("relay_failed")?;
    let snapshot = state.snapshot.lock().unwrap().clone();
    let peers = state.roster_seen.lock().unwrap().peers.clone();
    let dir = settings::app_dir();
    let paired = dir.as_deref().map(pairing::paired).unwrap_or_default();
    let aliases = tauri::async_runtime::spawn_blocking(ssh_aliases).await.unwrap_or_default();
    let mut hosts: Vec<(String, Option<String>)> = paired.iter().filter(|(id, _)| *id != here_id).map(|(id, host)| (host.clone(), Some(id.clone()))).collect();
    let mut left_out = Vec::new();
    let mut by_name = Vec::new();
    for peer in peers.iter().filter(|p| p.machine_id != here_id && !paired.iter().any(|(id, _)| *id == p.machine_id)) {
        let alias = peer.machine.trim().to_lowercase();
        if aliases.contains(&alias) {
            hosts.push((alias.clone(), Some(peer.machine_id.clone())));
            by_name.push((peer.machine_id.clone(), alias));
        } else {
            left_out.push(relay::Skipped::computer(&relay::shown_name(&peer.machine, &peer.machine_id), "no_ssh_host"));
        }
    }
    let asking = hosts.clone();
    let here = here_id.clone();
    let asked = tauri::async_runtime::spawn_blocking(move || relay::ask_all(Arc::new(pairing::Ssh), &asking, &here, monitoring, relay::ASK_LIMIT))
        .await.map_err(|_| "relay_failed".to_string())?;
    // Accounts this computer knows: its own and those the encrypted rosters name.
    let mut known: Vec<(Provider, String, Option<String>)> = snapshot.accounts.iter().map(|a| (a.provider, a.id.clone(), a.email.clone())).collect();
    for peer in &peers {
        known.extend(peer.accounts.iter().map(|e| (e.provider, e.id.clone(), e.email.clone())));
    }
    let reads_here = |provider: Provider, id: &str| snapshot.accounts.iter().any(|a| a.provider == provider && a.id == id && !a.id.starts_with("token:"));
    let name_here = |provider: Provider, id: &str| snapshot.accounts.iter().find(|a| a.provider == provider && a.id == id)
        .and_then(|a| a.alias.clone().or_else(|| a.email.clone()));
    // This computer's readings reach another only through the encrypted rosters: with readings
    // shared here, to a computer whose roster this one reads (so it reads this one's).
    let settings = state.settings.lock().unwrap().clone();
    let shares = settings.account_roster && settings.share_readings;
    let mut in_use: Vec<relay::InUse> = in_use_ending(&snapshot, chrono::Utc::now()).into_iter().map(|u| relay::InUse { machine: here_name.clone(), ..u }).collect();
    let mut others = Vec::new();
    let mut seen = HashSet::new();
    for answer in asked {
        match answer {
            relay::Asked::Here => {}
            relay::Asked::Skipped(skipped) => left_out.push(skipped),
            relay::Asked::Offer(offer) => {
                if !seen.insert(offer.machine.id.clone()) {
                    continue;
                }
                if let (Some(dir), Some((_, alias))) = (dir.as_deref(), by_name.iter().find(|(id, _)| *id == offer.machine.id)) {
                    pairing::remember_new_host(dir, &offer.machine.id, alias);
                }
                // Only accounts this computer knows are named to the person, as it knows them.
                in_use.extend(relay::known_in_use(&offer.in_use, &known));
                let shares_with_it = shares && peers.iter().any(|p| p.machine_id == offer.machine.id);
                let mut kept = Vec::new();
                for step in offer.plan.iter().flat_map(|g| g.steps.iter()) {
                    match relay::keep(step, &offer, &known, reads_here, shares_with_it) {
                        // Named as this computer names the account (its alias), else by its email.
                        relay::Keep::Yes => kept.push(bulk::Step { target: bulk::Target { account: name_here(step.target.provider, &step.target.id)
                            .or_else(|| step.email.clone()), ..step.target.clone() }, ..step.clone() }),
                        relay::Keep::Unknown => left_out.push(relay::Skipped {
                            account: step.target.account.as_deref().map(|a| relay::shown_name(a, "?")),
                            ..relay::Skipped::computer(&offer.machine.name, "relay_unknown_account") }),
                        relay::Keep::Covered => {}
                    }
                }
                others.push((offer.machine, vec![bulk::Group { email: None, steps: kept }]));
            }
        }
    }
    others.sort_by(|(a, _), (b, _)| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let groups = bulk::everywhere(sign_in_plan(app, monitoring), others);
    let computers = relay::computers(&groups, (&here_id, &here_name));
    let plan = Everywhere { id: uuid::Uuid::new_v4().simple().to_string(), at: Instant::now(), monitoring, groups, computers, here: here_id, left_out, in_use };
    *state.everywhere.lock().unwrap() = Some(plan.clone());
    Ok(plan)
}

/// Starts a gathered sitting (see `gather_everywhere`) as one run of "Sign in all accounts" whose
/// other computers' sign-ins are completed here (see `relayed_turn`).
fn start_everywhere(app: &AppHandle, plan: Everywhere) -> Result<(), String> {
    desktop_shown()?;
    run_sign_ins(app, plan.groups, true, plan.monitoring, Some(Gathered { left_out: plan.left_out, in_use: plan.in_use }))
}

/// An agent's confirm (`sign_in_all` with `everywhere`): a notification asks the person at this
/// screen to review the sitting; Review (or the notification itself) opens Settings > Accounts with
/// the same confirmation the person's own click shows (see `confirm_everywhere_here`), so no agent
/// relays pages without someone here agreeing to exactly what each computer will sign in. False
/// when the notification could not be shown.
fn ask_everywhere(app: &AppHandle, plan: &Everywhere) -> bool {
    let n: usize = plan.computers.iter().map(|c| c.steps).sum();
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    let lang = crate::i18n::resolve(&settings.language);
    let list = plan.computers.iter().map(|c| format!("{} ({})", c.name, c.steps)).collect::<Vec<_>>().join(", ");
    let title = crate::i18n::t(&lang, "signInEverywhere", &[]);
    let body = crate::i18n::t(&lang, "signInEverywhereAsk", &[("n", &n.to_string()), ("list", &list)]);
    let buttons = vec![(crate::i18n::t(&lang, "signInEverywhereStart", &[]), "review")];
    let (h, monitoring) = (app.clone(), plan.monitoring);
    notify::notify_choice(app.clone(), title, body, buttons, move |action| match action {
        Some("review") | None => confirm_everywhere_here(&h, monitoring),
        Some(_) => {}
    }).is_ok()
}

/// Opens Settings > Accounts on "Sign in all your computers from here"'s confirmation: it checks
/// every computer again and shows what each will sign in, and starts only on the person's OK. How
/// an agent's request and Retry of such a sitting start, so every sitting is one the person saw.
fn confirm_everywhere_here(app: &AppHandle, monitoring: bool) {
    // Read by the page as it loads, or at once by one already open.
    *app.state::<AppState>().everywhere_ask.lock().unwrap() = Some((monitoring, Instant::now()));
    let _ = app.emit_to(SETTINGS_LABEL, "sign-in-everywhere-ask", ());
    show_settings_page(app, Some("accounts".into()));
}

/// Settings > Accounts asks whether a sitting waits for the person's confirmation (see
/// `confirm_everywhere_here`): its monitoring switch, once, while the ask is fresh (an ask left
/// from long ago never opens a confirmation by surprise).
#[tauri::command]
fn take_everywhere_ask(state: tauri::State<AppState>) -> Option<bool> {
    state.everywhere_ask.lock().unwrap().take().filter(|(_, at)| at.elapsed() < EVERYWHERE_FRESH).map(|(monitoring, _)| monitoring)
}

/// Where a relayed sign-in's page opens here: the email's fresh profile (made now if its turn has
/// none yet), or a private window when the user chose those; never the everyday browser, whose
/// history would keep the provider's answer to the page.
fn relay_session(app: &AppHandle, group_session: &mut Option<Option<Arc<signin::Session>>>, email: Option<&str>, lang: &str) -> Option<Arc<signin::Session>> {
    if let Some(Some(session)) = group_session {
        return Some(session.clone());
    }
    if app.state::<AppState>().settings.lock().unwrap().signin_window == signin::Window::Private {
        return None;
    }
    let session = signin::Session::create().map(Arc::new);
    if let (Some(s), Some(email)) = (session.as_deref(), email) {
        s.label(email, lang);
    }
    *group_session = Some(session.clone());
    session
}

/// "{what} for {machine}": a sign-in of another computer's, as notifications and pages name it.
fn for_machine(lang: &str, what: &str, machine: &str) -> String {
    crate::i18n::t(lang, "signInFor", &[("what", what), ("machine", machine)])
}

/// The computers (and sign-ins) left out, in words: "PC-MINI1 (not reachable over SSH)".
fn left_out_text(lang: &str, left: &[relay::Skipped]) -> String {
    left.iter().map(|s| {
        let why = crate::i18n::t(lang, s.why, &[]);
        match &s.account {
            Some(account) => format!("{} ({}: {why})", s.machine, account),
            None => format!("{} ({why})", s.machine),
        }
    }).collect::<Vec<_>>().join(", ")
}

/// This computer as `relay::drive` uses it: the user's own SSH to the other computer, the tunnel,
/// and this computer's browser in the email's window.
struct AppDesk {
    target: signin::Target,
}

impl relay::Desk for AppDesk {
    fn call(&self, host: &str, request: &door::Request, wait: Duration) -> Result<serde_json::Value, String> {
        pairing::relay_call(&pairing::Ssh, host, request, wait)
    }

    fn tunnel(&self, host: &str, page: &relay::Page) -> Result<Box<dyn relay::Hold>, relay::Unheld> {
        relay::Tunnel::open(&pairing::Ssh::exe(), host, page).map(|t| Box::new(t) as Box<dyn relay::Hold>)
    }

    fn open(&self, url: &str) -> Result<(), String> {
        signin::open(url, &self.target)
    }
}

/// What a run's turn at one sign-in answers to: Stop, "Retry this sign-in" (the run's own,
/// asked for its `at`th sign-in), and the account its notifications name.
struct Turn<'a> {
    stop: &'a tokio::sync::Notify,
    restart: &'a Arc<StepRestart>,
    at: usize,
    account: &'a str,
}

/// One sign-in of another computer's (see `relay`), completed in this computer's browser: in the
/// email's fresh profile (`relay_session`), or a private window when the user chose those. It
/// starts there, its page opens here once that computer's CLI hands it over and a tunnel for its
/// port is up (`on_page(true)` then), and it is followed until it ends. Stop, closing the browser
/// and the long-wait notice work as for a sign-in here, and so does "Retry this sign-in" (the
/// one there stops, a new one starts, `on_page(false)`); dropping it closes the tunnel at once and
/// asks that computer to stop. This computer holds its one sign-in (`monitoring::SIGN_IN`) and the
/// open sign-in's mark, which puts updates off, while it runs. Returns as `relay::drive` does.
async fn relayed_turn(h: &AppHandle, step: &bulk::Step, machine: &bulk::Machine, session: Option<&Arc<signin::Session>>, turn: &Turn<'_>,
    mut on_page: impl FnMut(bool)) -> Result<bool, String> {
    let stop = turn.stop;
    let window = h.state::<AppState>().settings.lock().unwrap().signin_window;
    let target = match (window, session) {
        (signin::Window::Private, _) => signin::Target::Private,
        (_, Some(s)) => s.target(),
        (_, None) => return Err("signin_profile_storage".into()),
    };
    let from = presence::identity().map(|(id, _)| id.to_string()).ok_or("relay_failed")?;
    // This computer's one sign-in: waited for (another may have started since the run's own wait),
    // never a reason to leave that computer out; Stop ends the wait.
    let _sign_in = loop {
        if let Ok(held) = h.state::<AppState>().account_operations.begin_key(monitoring::SIGN_IN) {
            break held;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = stop.notified() => return Err("signin_stopped".into()),
        }
    };
    let _open = h.state::<AppState>().sign_ins.open();
    let stopper: Arc<dyn Fn(&str, &str) + Send + Sync> = Arc::new(|host: &str, step: &str| {
        let (host, step) = (host.to_owned(), step.to_owned());
        std::thread::spawn(move || {
            let _ = pairing::relay_call(&pairing::Ssh, &host, &door::Request::RelayStop { step }, Duration::ZERO);
        });
    });
    let relayed = relay::Relayed::new(&machine.host, stopper);
    let (shared, host, ids, owned) = (relayed.shared.clone(), machine.host.clone(), relay::Ids { from, machine: machine.id.clone() }, step.clone());
    // How many pages it opened here: one more at each start again.
    let pages = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counted = pages.clone();
    let desk = AppDesk { target };
    let task = tauri::async_runtime::spawn_blocking(move || relay::drive(&desk, &host, &ids, &owned, &shared, &|| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let mut run = Box::pin(async move { task.await.unwrap_or_else(|_| Err("relay_failed".into())) });
    if let Some(s) = session {
        s.rearm();
    }
    let closed = browser_closed(session.map(|s| &**s));
    tokio::pin!(closed);
    // No page limit here: the other computer has its own (`relay::PAGE_LIMIT`).
    turn.restart.begin();
    let mut watch = Box::pin(watch_step(h, None, std::time::SystemTime::now(), true, turn));
    // The pages shown open so far.
    let mut shown = 0;
    let outcome = loop {
        let opened = {
            let pages = pages.clone();
            async move {
                while pages.load(std::sync::atomic::Ordering::SeqCst) <= shown {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        };
        tokio::select! {
            // In this order, as for a sign-in here: a finished one, or Stop, wins.
            biased;
            outcome = &mut run => break outcome,
            _ = stop.notified() => break Err("signin_stopped".into()),
            // As for a sign-in here: the provider's "done" page closed while the other computer saves.
            _ = &mut closed => {
                publish_bulk(h, |b| b.still_waiting = false);
                break tokio::time::timeout(SIGN_IN_GRACE, &mut run).await.unwrap_or_else(|_| Err("signin_window_closed".into()));
            }
            e = &mut watch => break Err(e),
            _ = opened => {
                shown = pages.load(std::sync::atomic::Ordering::SeqCst);
                on_page(true);
            }
            _ = turn.restart.notify.notified() => {
                if !turn.restart.take(turn.at) {
                    continue;
                }
                // Asked for again: the one there stops, and a new one starts whose page opens here,
                // in the same window. A page the old one opened just now is not shown open.
                relayed.restart();
                shown = pages.load(std::sync::atomic::Ordering::SeqCst);
                on_page(false);
                watch = Box::pin(watch_step(h, None, std::time::SystemTime::now(), true, turn));
                publish_bulk(h, |b| b.still_waiting = false);
                if let Some(s) = session {
                    s.rearm();
                }
                turn.restart.started();
            }
        }
    };
    // Closes the tunnel, and stops the step there unless it ended there by itself.
    drop(relayed);
    outcome
}

/// Settings > Accounts, "Sign in all your computers from here": what it would run (see
/// `gather_everywhere`), for the person's OK.
#[tauri::command]
async fn plan_sign_in_everywhere(app: AppHandle, monitoring: bool) -> Result<Everywhere, String> {
    gather_everywhere(&app, monitoring).await
}

/// The person's OK on that plan: runs exactly it, while it is fresh, and once.
#[tauri::command]
fn start_sign_in_everywhere(app: AppHandle, id: String) -> Result<(), String> {
    let state = app.state::<AppState>();
    let plan = state.everywhere.lock().unwrap().take_if(|p| p.id == id && p.at.elapsed() < EVERYWHERE_FRESH).ok_or("signin_plan_expired")?;
    start_everywhere(&app, plan)
}

/// The notification text for the person at this desktop during "Sign in all accounts", in the
/// words Settings > Accounts uses: (title, body, language).
fn sign_in_text(app: &AppHandle, key: &str, vars: &[(&str, &str)]) -> (String, String, String) {
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    let lang = crate::i18n::resolve(&settings.language);
    let place = if settings.signin_window == signin::Window::System { "signInWhereTerminal" } else { "signInWhereBrowser" };
    let place = crate::i18n::t(&lang, place, &[]);
    let mut vars = vars.to_vec();
    vars.push(("where", &place));
    (crate::i18n::t(&lang, "signInAll", &[]), crate::i18n::t(&lang, key, &vars), lang)
}

/// A notification for the person at this desktop; clicking it opens Settings > Accounts.
fn sign_in_notice(app: &AppHandle, key: &str, vars: &[(&str, &str)]) {
    let (title, body, _) = sign_in_text(app, key, vars);
    let _ = notify::notify(app.clone(), title, body, Some("accounts".into()), None);
}

/// The same for a run that stopped, with a Retry button (see `retry_run`).
fn sign_in_choice(app: &AppHandle, key: &str, vars: &[(&str, &str)]) {
    let (title, body, lang) = sign_in_text(app, key, vars);
    let buttons = vec![(crate::i18n::t(&lang, "signInRetry", &[]), "retry")];
    let h = app.clone();
    let _ = notify::notify_choice(app.clone(), title, body, buttons, move |action| match action {
        Some("retry") => {
            let h = h.clone();
            tauri::async_runtime::spawn(async move {
                // A failure shows in Settings and readiness like any run that did not start.
                let _ = retry_run(&h).await;
            });
        }
        _ => show_settings_page(&h, Some("accounts".into())),
    });
}

/// The notification that run `run`'s open sign-in (its `at`th) still waits on the person: Retry
/// starts that sign-in again in the same window (see `StepRestart`), as its text says, and the run
/// goes on; Stop ends the run.
fn still_waiting_choice(app: &AppHandle, vars: &[(&str, &str)], run: &Arc<StepRestart>, at: usize) {
    let (title, body, lang) = sign_in_text(app, "signInStillWaiting", vars);
    let buttons = vec![(crate::i18n::t(&lang, "signInRetry", &[]), "retry"), (crate::i18n::t(&lang, "signInAllStop", &[]), "stop")];
    let (h, run) = (app.clone(), run.clone());
    let _ = notify::notify_choice(app.clone(), title, body, buttons, move |action| match action {
        // One left from a sign-in that has ended opens Settings, where the run's state is.
        Some("retry") => {
            if !restart_open_sign_in(&h, at, Some(&run)) {
                show_settings_page(&h, Some("accounts".into()));
            }
        }
        Some("stop") => stop_running_sign_ins(&h.state::<AppState>()),
        _ => show_settings_page(&h, Some("accounts".into())),
    });
}

/// Starts the running run's open sign-in again (see `StepRestart`): only while it is open and is
/// the run's `at`th (and, with `run`, only in that run: a notification left from an earlier run
/// starts nothing). Whether it was asked.
fn restart_open_sign_in(app: &AppHandle, at: usize, run: Option<&Arc<StepRestart>>) -> bool {
    let state = app.state::<AppState>();
    let open = state.bulk_connect.lock().unwrap().as_ref().filter(|b| !b.finished)
        .and_then(|b| b.steps.iter().position(|s| s.state == StepState::Running));
    if open != Some(at) {
        return false;
    }
    let Some(current) = state.bulk_restart.lock().unwrap().clone() else { return false };
    if run.is_some_and(|run| !Arc::ptr_eq(run, &current)) {
        return false;
    }
    current.ask(at);
    true
}

/// Settings > Accounts, "Retry this sign-in" on the run's open sign-in, its `at`th: see
/// `restart_open_sign_in`.
#[tauri::command]
fn restart_sign_in(app: AppHandle, at: usize) -> bool {
    restart_open_sign_in(&app, at, None)
}

/// Why a sign-in asked for again from elsewhere did not start again (see `restartable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum NotRestarted {
    /// No run goes here: it ended, or none ran since SlopBar started.
    NoRun,
    /// The run signs in every computer from this screen (see `relay`): its open sign-in may be
    /// another computer's, which a request from elsewhere must never reach on to, so only the
    /// person here starts it again (Settings' "Retry this sign-in").
    Everywhere,
    /// Another run goes here now.
    OtherRun,
    /// That sign-in is not the open one: it ended, or has not started.
    NotOpen,
    /// It has not waited long: the person here may still be completing it.
    NotWaiting,
}

/// Whether sign-in `step` of the run that started at `run_started_at` may start again from elsewhere
/// (the door's `restart_sign_in`: another computer's Retry on its card, or an agent): that run goes
/// on, is not a sitting for every computer, `step` is its open sign-in, and it has waited long enough
/// that the person here was asked whether to retry it (`still_waiting`), so it is never cut off in
/// its first 5 minutes from another computer. Settings' own "Retry this sign-in" and the
/// notification's Retry, at this screen, need no wait.
fn restartable(run: Option<&BulkConnect>, step: usize, run_started_at: chrono::DateTime<chrono::Utc>) -> Result<(), NotRestarted> {
    let run = run.filter(|b| !b.finished).ok_or(NotRestarted::NoRun)?;
    if run.everywhere {
        return Err(NotRestarted::Everywhere);
    }
    if run.run_started_at != run_started_at {
        return Err(NotRestarted::OtherRun);
    }
    if run.steps.iter().position(|s| s.state == StepState::Running) != Some(step) {
        return Err(NotRestarted::NotOpen);
    }
    if !run.still_waiting {
        return Err(NotRestarted::NotWaiting);
    }
    Ok(())
}

/// The door's `restart_sign_in`: asks the run named by `run_started_at` to start its open sign-in
/// `step` again in the same window (see `StepRestart`) when `restartable` says it may. The run and its
/// restart are read under one hold of `bulk_connect`, which a new run takes to set both, so the ask
/// reaches the run that was judged; a run that moves on meanwhile takes no ask for another sign-in.
fn restart_from_elsewhere(app: &AppHandle, step: usize, run_started_at: chrono::DateTime<chrono::Utc>) -> Result<(), NotRestarted> {
    let state = app.state::<AppState>();
    let running = state.bulk_connect.lock().unwrap();
    restartable(running.as_ref(), step, run_started_at)?;
    let restart = state.bulk_restart.lock().unwrap().clone().ok_or(NotRestarted::NoRun)?;
    restart.ask(step);
    Ok(())
}

/// What `restart_sign_in` tells the agent next. Each fits `mcp::MAX_TEXT`, so it reaches the agent whole.
const RESTARTED_NEXT: &str = "That sign-in starts again in the same window: its old page closes, a new one opens and the run \
    goes on. The person at this screen completes it, never you. Follow it with get_machine_readiness (wait_seconds and since); \
    starting it again once more needs another 5 minutes of waiting.";
fn not_restarted_next(why: NotRestarted) -> &'static str {
    match why {
        NotRestarted::NoRun => "Nothing was restarted: no sign-in run goes on here (it ended, or none ran since the app started). \
            This tool never starts one; call sign_in_all only when the user asks for it.",
        NotRestarted::Everywhere => "Nothing was restarted: this run signs in every computer from this screen, so only the person \
            here starts one of its sign-ins again, with Retry this sign-in in Settings > Accounts.",
        NotRestarted::OtherRun => "Nothing was restarted: another run goes on here than the one run_started_at names (a step's \
            own started_at names none). Read get_machine_readiness again, and restart only if the user still wants it.",
        NotRestarted::NotOpen => "Nothing was restarted: that step is not the open sign-in (it finished, or has not started). \
            The open one is the step whose state is running, counted from 0 in get_machine_readiness's sign_in.steps.",
        NotRestarted::NotWaiting => "Nothing was restarted: that sign-in has not waited 5 minutes since it opened (or it was just \
            started again), so someone may be completing it. Do not loop: wait for sign_in.still_waiting (get_machine_readiness \
            with wait_seconds and since), or ask the user to choose Retry this sign-in at this screen.",
    }
}

#[tauri::command]
async fn connect_monitoring_all(app: AppHandle) -> Result<Vec<bulk::Target>, String> {
    connect_all(&app).await
}

/// What the bulk actions would act on now, and how a running "Connect for all" is doing.
#[tauri::command]
fn bulk_status(state: tauri::State<AppState>) -> serde_json::Value {
    let snapshot = state.snapshot.lock().unwrap().clone();
    let settings = state.settings.lock().unwrap().clone();
    serde_json::json!({
        // "Connect monitoring for all": accounts without monitoring, or whose sign-in ended,
        // was refused or ends soon.
        "to_connect": bulk::to_connect(&snapshot),
        "lapsed": bulk::lapsed(&snapshot, &settings, chrono::Utc::now()),
        "connecting": state.bulk_connect.lock().unwrap().clone(),
        // "Sign in all accounts" with monitoring; without it, only the `add` and `renew` steps run.
        "sign_in": bulk::sign_in_plan(&snapshot, &pending_accounts(&state, &snapshot), true),
        "roster_waiting": roster_waiting(&state, &snapshot).len(),
        "signin_profiles": signin::count(),
        // Other computers "Sign in all your computers from here" can ask (see `relay`).
        "other_computers": other_computers(&state),
    })
}

#[tauri::command]
async fn sign_in_all_accounts(app: AppHandle, monitoring: bool) -> Result<Vec<bulk::Group>, String> {
    sign_in_all(&app, monitoring).await
}

/// "Clear old sign-in profiles": deletes the profiles earlier versions kept per email, and any a
/// stopped SlopBar left, with their cookies (see `signin::cleanup`).
#[tauri::command]
async fn forget_signin_profiles() -> Result<usize, String> {
    tauri::async_runtime::spawn_blocking(signin::cleanup).await.map_err(|_| "signin_profiles_busy".to_string())?
}

/// Stops a running "Sign in all accounts" or "Connect monitoring for all": the open sign-in is
/// closed with its browser, and nothing after it starts.
#[tauri::command]
fn stop_sign_ins(state: tauri::State<AppState>) {
    stop_running_sign_ins(&state);
}

fn stop_running_sign_ins(state: &AppState) {
    if let Some(stop) = state.bulk_stop.lock().unwrap().as_ref() {
        stop.notify_one();
    }
}

/// Settings' Retry: see `retry_run`.
#[tauri::command]
async fn retry_sign_ins(app: AppHandle) -> Result<(), String> {
    retry_run(&app).await
}

/// Removes every account whose plan has ended, as each row's remove does; accounts in use stay.
async fn remove_lapsed(app: &AppHandle) -> serde_json::Value {
    let plan = {
        let state = app.state::<AppState>();
        let snapshot = state.snapshot.lock().unwrap().clone();
        let settings = state.settings.lock().unwrap().clone();
        bulk::lapsed(&snapshot, &settings, chrono::Utc::now())
    };
    let (mut removed, mut failed) = (Vec::new(), Vec::new());
    for target in &plan.remove {
        match remove_one(app, target.provider, &target.id).await {
            Ok(()) => removed.push(target.clone()),
            Err(error) => failed.push(serde_json::json!({ "account": target, "error": error })),
        }
    }
    let snapshot = do_refresh(app).await;
    serde_json::json!({ "removed": removed, "failed": failed, "in_use": plan.in_use, "snapshot": snapshot })
}

#[tauri::command]
async fn remove_lapsed_accounts(app: AppHandle) -> serde_json::Value {
    remove_lapsed(&app).await
}

#[tauri::command]
async fn remove_account(app: AppHandle, provider: Provider, id: String) -> Result<Snapshot, String> {
    remove_one(&app, provider, &id).await?;
    Ok(do_refresh(&app).await)
}

/// Removes a saved account that is not in use: its monitoring sign-in, its project pins and its
/// profile's login (the rest of the profile folder stays), then the saved login.
async fn remove_one(app: &AppHandle, provider: Provider, id: &str) -> Result<(), String> {
    let id = id.to_string();
    let operation = app.state::<AppState>().account_operations.begin(provider, &id)?;
    let active = {
        let snapshot = app.state::<AppState>().snapshot.lock().unwrap().clone();
        snapshot.accounts.iter().any(|a| a.provider == provider && a.id == id && a.active)
    };
    if active {
        // It would be captured again from the CLI on the next refresh.
        return Err("active".into());
    }
    if let Some(slot) = accounts::load_slots().into_iter().find(|s| s.provider == provider && s.id == id) {
        app.state::<AppState>().monitor.disconnect(&slot, &http_client()).await?;
        // Dropped by the next refresh, under the refresh lock: one running now would write it back.
        app.state::<AppState>().cache_due.forget(monitoring::key(&slot));
    }
    {
        let state = app.state::<AppState>();
        let held = state.settings_lock.lock().await;
        let current = state.settings.lock().unwrap().clone();
        if current.project_pins.iter().any(|p| p.provider == provider && p.id == id) {
            let project_pins = current.project_pins.iter().filter(|p| !(p.provider == provider && p.id == id)).cloned().collect();
            if store_settings(app, held, Settings { project_pins, ..current })? {
                refresh_soon(app);
            }
        }
    }
    if let Some(root) = accounts::profiles_root() {
        accounts::withdraw_profile(provider, &root.join(accounts::profile_name(provider, &id))).map_err(|e| e.to_string())?;
    }
    accounts::remove_slot(provider, &id).map_err(|e| e.to_string())?;
    drop(operation);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Project pins (see `pins`)

/// Signs each pinned account's profile in or out (see `accounts::sync_profiles`) and rewrites the
/// launcher. `active` is where accounts are signed in by default, when the caller just read it.
async fn sync_pins(app: &AppHandle, active: Option<accounts::Active>) {
    let state = app.state::<AppState>();
    let pins = state.settings.lock().unwrap().project_pins.clone();
    let wanted = profile_accounts(&state);
    let found = state.sources.lock().unwrap().clone();
    let states = tauri::async_runtime::spawn_blocking(move || {
        // Never decide what is signed in by default from an empty source list.
        let found = if found.is_empty() { sources::discover() } else { found };
        let active = active.unwrap_or_else(|| accounts::capture(&found));
        let states = accounts::sync_profiles(&wanted, &active);
        // The launcher also serves folders without pins (see `launcher`).
        let _ = pins::write_launcher(&pins);
        states
    }).await.unwrap_or_default();
    *state.profile_states.lock().unwrap() = states;
}

/// The accounts whose project profiles stay signed in: pinned ones and leased ones (a leased
/// account's job may run with its profile, see `leases`).
fn profile_accounts(state: &AppState) -> Vec<(Provider, String)> {
    let mut wanted = pins::wanted(&state.settings.lock().unwrap().project_pins);
    for account in state.leases.lock().unwrap().accounts() {
        if !wanted.contains(&account) { wanted.push(account); }
    }
    wanted
}

// ---------------------------------------------------------------------------------------------
// Leases (see `leases`)

/// recommend_account's answer without the accounts leased by anyone but `mine`.
fn recommend_unleased(app: &AppHandle, provider: Provider, expected_percent: Option<f64>, hours: Option<f64>, mine: Option<&str>) -> headroom::Recommendation {
    let state = app.state::<AppState>();
    let snapshot = state.snapshot.lock().unwrap().clone();
    let settings = state.settings.lock().unwrap().clone();
    let rec = headroom::recommend(&snapshot.accounts, &settings, provider, expected_percent, hours, chrono::Utc::now());
    let leased = state.leases.lock().unwrap().others(provider, mine);
    headroom::without_leased(rec, &leased)
}

/// Shows each account's lease on the snapshot (the flyout's "leased: <purpose>").
fn mark_leases(state: &AppState, snapshot: &mut Snapshot) {
    let leases = state.leases.lock().unwrap();
    for a in &mut snapshot.accounts {
        a.leased = leases.holder(a.provider, &a.id).map(|l| l.purpose.clone());
    }
}

/// After leases change: the windows show them and leased accounts' profiles are signed in or out.
async fn leases_changed(app: &AppHandle) {
    let state = app.state::<AppState>();
    let snapshot = {
        let mut snapshot = state.snapshot.lock().unwrap().clone();
        mark_leases(&state, &mut snapshot);
        *state.snapshot.lock().unwrap() = snapshot.clone();
        snapshot
    };
    let _ = app.emit("usage-updated", &snapshot);
    crate::alerts::on_snapshot(&app);
    sync_pins(app, None).await;
}

/// Ends expired leases and those whose process exited.
async fn prune_leases(app: &AppHandle) {
    let ended = app.state::<AppState>().leases.lock().unwrap().prune(chrono::Utc::now(), leases::process_alive);
    if ended { leases_changed(app).await; }
}

/// update_settings' error for an alias of no account: the key the agent sent last, so a long one
/// cuts only itself where agents read it (`mcp::MAX_TEXT`), never what to do.
fn no_alias_account(key: &str) -> String {
    format!("alias keys are \"provider:id\" from get_usage; no account has the key {key:?}")
}

/// Leases account `id` (else the recommended one among those nobody leased) for `purpose`, and
/// says how to run on it: `config_dir` is its project profile, or null while it is the CLI's
/// current sign-in.
async fn lease_account(app: &AppHandle, provider: Provider, purpose: &str, minutes: Option<u32>, expected_percent: Option<f64>, id: Option<String>, pid: Option<u32>) -> Result<serde_json::Value, String> {
    prune_leases(app).await;
    let state = app.state::<AppState>();
    let (id, label, active) = match id {
        Some(id) => {
            let snapshot = state.snapshot.lock().unwrap();
            let account = snapshot.accounts.iter().find(|a| a.provider == provider && a.id == id).ok_or("monitor_wrong_account")?;
            // A job would run on a saved CLI sign-in that ended, and stop until it signs in again.
            if !account.active && account.cli_ends_at.is_some_and(|t| t <= chrono::Utc::now()) {
                return Err("cli_signin_ended: this account's saved CLI sign-in ended, so a job on it would stop until it signs in again; sign_in_all signs it in again (the user completes the page), or lease another account".into());
            }
            (id, account.alias.clone().or(account.email.clone()), account.active)
        }
        None => {
            let rec = recommend_unleased(app, provider, expected_percent, None, None);
            let id = rec.recommended.clone().ok_or_else(|| format!("no_account_free: {}", rec.summary))?;
            let active = rec.ranking.iter().any(|c| c.id == id && c.active);
            (id, rec.recommended_label.clone(), active)
        }
    };
    let lease = state.leases.lock().unwrap().take(provider, &id, purpose, minutes, pid, chrono::Utc::now())?;
    leases_changed(app).await;
    let profile = accounts::profiles_root().map(|r| r.join(accounts::profile_name(provider, &id)));
    let ready = state.profile_states.lock().unwrap().get(&(provider, id.clone())) == Some(&accounts::ProfileState::Ready);
    let config_dir = profile.filter(|_| ready && !active).map(|p| p.to_string_lossy().into_owned());
    Ok(serde_json::json!({
        "lease": serde_json::to_value(&lease).map_err(|e| e.to_string())?,
        "id": id, "label": label, "expires_at": lease.expires_at,
        "env_var": pins::env_var(provider), "config_dir": config_dir, "at_default": active,
        "next": "Run the job with env_var set to config_dir for that job only (null: the account is the CLI's current sign-in), then release_lease.",
    }))
}

// ---------------------------------------------------------------------------------------------
// Exact Claude attribution (see `session_hook`)

/// Records which Claude account a session started on: the account signed in to its config dir,
/// or with none given (the default login), the account SlopBar sees signed in to Claude Code.
async fn record_session(app: &AppHandle, session_id: String, cwd: Option<String>, config_dir: Option<String>) -> Result<serde_json::Value, String> {
    if !crate::session_hook::valid_session_id(&session_id) {
        return Err("bad_session_id".into());
    }
    let state = app.state::<AppState>();
    let default = || {
        let snapshot = state.snapshot.lock().unwrap();
        active_log::active_ids(&snapshot.accounts).into_iter().find(|(p, _)| *p == Provider::Claude).map(|(_, id)| id.to_string())
    };
    let id = match config_dir.filter(|d| std::path::Path::new(d).is_absolute()) {
        Some(dir) => tauri::async_runtime::spawn_blocking(move || accounts::login_at(Provider::Claude, std::path::Path::new(&dir))).await.ok().flatten(),
        None => default(),
    };
    let Some(id) = id else { return Ok(serde_json::json!({ "recorded": false, "reason": "unknown_account" })) };
    let now = chrono::Utc::now();
    let changed = state.active_log.lock().unwrap().record_session(&session_id, &id, cwd.as_deref().unwrap_or(""), now);
    if changed {
        let log = state.active_log.lock().unwrap().clone();
        let _ = tauri::async_runtime::spawn_blocking(move || active_log::save(&log)).await;
    }
    Ok(serde_json::json!({ "recorded": true }))
}

/// Claude Code's turn hook (see `turn_hook`, `wind_down`). Only sessions on the default login
/// take part: a switch moves the default login, never a session's own config dir.
async fn record_turn(app: &AppHandle, session_id: String, event: String, config_dir: Option<String>) -> Result<serde_json::Value, String> {
    let event = match event.as_str() {
        "prompt" => wind_down::Event::Prompt,
        "stop" => wind_down::Event::Stop,
        _ => return Err("bad_event".into()),
    };
    if !crate::session_hook::valid_session_id(&session_id) {
        return Err("bad_session_id".into());
    }
    let none = || Ok(serde_json::json!({ "context": null, "block": null }));
    if config_dir.is_some() {
        return none();
    }
    let state = app.state::<AppState>();
    // The account it started on (SessionStart hook); a session SlopBar has not heard of yet is
    // on the default login's account now.
    let known = state.active_log.lock().unwrap().session(&session_id).map(str::to_string);
    let account = match known {
        Some(id) => id,
        None => {
            record_session(app, session_id.clone(), None, None).await?;
            let Some(id) = state.active_log.lock().unwrap().session(&session_id).map(str::to_string) else { return none() };
            id
        }
    };
    let now = chrono::Utc::now();
    let (reply, ready) = {
        let mut tracker = state.wind_down.lock().unwrap();
        let reply = tracker.on_event(&session_id, &account, event, now);
        (reply, event == wind_down::Event::Stop && tracker.ready(now))
    };
    if ready {
        let app = app.clone();
        tauri::async_runtime::spawn(async move { finish_wind_down(&app).await });
    }
    Ok(match reply {
        wind_down::Reply::None => serde_json::json!({ "context": null, "block": null }),
        wind_down::Reply::Context(c) => serde_json::json!({ "context": c, "block": null }),
        wind_down::Reply::Block(b) => serde_json::json!({ "context": null, "block": b }),
    })
}

/// The balancer's switches, with a Claude account that has its own percentage (`rotate_at`) held
/// back while its sessions wind down (see `wind_down`): the wind-down starts at that percentage,
/// and the switch goes ahead once it is ready or usage reaches the hard limit. A wind-down whose
/// reason went away (a manual switch, a reset, no account to move to) ends without switching.
fn graceful_decisions(app: &AppHandle, list: &[model::AccountUsage], settings: &Settings, decisions: Vec<(Provider, String)>) -> Vec<(Provider, String)> {
    let state = app.state::<AppState>();
    let now = chrono::Utc::now();
    let current = list.iter().find(|a| a.provider == Provider::Claude && a.active);
    let own = current.and_then(|a| settings.rotate_at.get(&format!("claude:{}", a.id)).copied());
    let target = decisions.iter().find(|(p, _)| *p == Provider::Claude).map(|(_, id)| id.clone());
    let mut tracker = state.wind_down.lock().unwrap();
    let (Some(target), Some(current), Some(own)) = (target, current, own) else {
        tracker.finish();
        return decisions;
    };
    let peak = current.peak().unwrap_or(0.0);
    let hold = peak < wind_down::hard_limit(own);
    let mut started = None;
    if hold && tracker.active.as_ref().is_none_or(|a| a.from != current.id) {
        tracker.start(&current.id, chrono::Duration::minutes(settings.rotate_grace_minutes as i64), now);
        started = tracker.active.as_ref().map(|a| a.deadline);
    }
    if hold && !tracker.ready(now) {
        drop(tracker);
        if let Some(deadline) = started {
            let label = |id: &str| list.iter().find(|a| a.provider == Provider::Claude && a.id == id).and_then(|a| a.alias.clone().or(a.email.clone()));
            crate::alerts::on_wind_down(app, label(&current.id).as_deref(), label(&target).as_deref(), peak);
            let wait = (deadline - now).to_std().unwrap_or_default() + Duration::from_secs(1);
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(wait).await;
                finish_wind_down(&app).await;
            });
        }
        return decisions.into_iter().filter(|(p, _)| *p != Provider::Claude).collect();
    }
    tracker.finish();
    decisions
}

/// Ends a wind-down that is ready: switches to the account the balancer picks now (the one chosen
/// at the start may have changed), unless the account is no longer signed in or no longer needs it.
async fn finish_wind_down(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(done) = ({
        let mut tracker = state.wind_down.lock().unwrap();
        if tracker.ready(chrono::Utc::now()) { tracker.finish() } else { None }
    }) else { return };
    let (accounts, settings) = (state.snapshot.lock().unwrap().accounts.clone(), state.settings.lock().unwrap().clone());
    if !accounts.iter().any(|a| a.provider == Provider::Claude && a.active && a.id == done.from) {
        return;
    }
    let leased = state.leases.lock().unwrap().accounts();
    let Some((_, target)) = balancer::decide_avoiding(&accounts, &settings, &HashMap::new(), &leased)
        .into_iter().find(|(p, _)| *p == Provider::Claude) else { return };
    if switch_to(app, Provider::Claude, target, true).await.is_ok() {
        state.last_switch.lock().unwrap().insert(Provider::Claude, Instant::now());
        do_refresh(app).await;
    }
}

fn pin_listing(app: &AppHandle) -> pins::Listing {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let snapshot = state.snapshot.lock().unwrap().clone();
    let states = state.profile_states.lock().unwrap().clone();
    let label = |provider: Provider, id: &str| snapshot.accounts.iter()
        .find(|a| a.provider == provider && a.id == id)
        .and_then(|a| a.alias.clone().or(a.email.clone()));
    let root = accounts::profiles_root().unwrap_or_default();
    pins::listing(&settings.project_pins, &states, label, &root, pins::launcher_path().as_deref())
}

#[tauri::command]
fn list_project_pins(app: AppHandle) -> pins::Listing {
    pin_listing(&app)
}

#[tauri::command]
async fn set_project_pin(app: AppHandle, folder: String, provider: Provider, id: Option<String>) -> Result<pins::Listing, String> {
    {
        let state = app.state::<AppState>();
        let held = state.settings_lock.lock().await;
        let current = state.settings.lock().unwrap().clone();
        let known: Vec<_> = accounts::load_slots().into_iter().map(|s| (s.provider, s.id)).collect();
        let project_pins = pins::set(&current.project_pins, &folder, provider, id.as_deref(), &known)?;
        if store_settings(&app, held, Settings { project_pins, ..current })? {
            refresh_soon(&app);
        }
    }
    sync_pins(&app, None).await;
    Ok(pin_listing(&app))
}

// ---------------------------------------------------------------------------------------------
// The quick switcher

/// How long the switcher stays loaded after it hides, so opening it again soon is instant.
const SWITCHER_LINGER: Duration = Duration::from_secs(30);
/// How long after a blur the switcher checks whether it really lost the foreground.
const SWITCHER_BLUR_SETTLE: Duration = Duration::from_millis(80);

/// Settings and the quick switcher are not created at startup (`"create": false`): each is
/// built when first opened and destroyed when closed, so neither keeps a WebView2 renderer
/// while unused. Whatever must outlive them is kept here or in settings, not in the page. The
/// web taskbar widget is built only while the setting `native_widget` is off (`dock::apply_mode`),
/// and the flyout hidden shortly after startup, kept for instant opening (see `panel::prepare`).
pub(crate) const ON_DEMAND: [&str; 4] = [SETTINGS_LABEL, SWITCHER_LABEL, dock::LABEL, panel::LABEL];

/// The window `label` (one of `ON_DEMAND`), created from its configuration if it is not open.
/// Builds off the main thread: building a window there waits on the event loop it runs.
pub(crate) async fn on_demand_window(app: &AppHandle, label: &str) -> Option<tauri::WebviewWindow> {
    if let Some(w) = app.get_webview_window(label) {
        return Some(w);
    }
    let config = app.config().app.windows.iter().find(|w| w.label == label)?.clone();
    let h = app.clone();
    let built = tauri::async_runtime::spawn_blocking(move || tauri::WebviewWindowBuilder::from_config(&h, &config).ok()?.build().ok())
        .await.ok().flatten();
    // Two callers raced: the other one built it.
    let Some(win) = built else { return app.get_webview_window(label) };
    match label {
        SETTINGS_LABEL => watch_settings(&win),
        SWITCHER_LABEL => watch_switcher(&win),
        #[cfg(windows)]
        dock::LABEL => dock::exclude_from_peek(&win),
        panel::LABEL => panel::watch(app, &win),
        _ => {}
    }
    Some(win)
}

/// Closing Settings destroys it (the default); only its Mica backdrop is set here.
fn watch_settings(win: &tauri::WebviewWindow) {
    if dock::is_windows_11() {
        use tauri::window::{Effect, EffectsBuilder};
        let _ = win.set_effects(EffectsBuilder::new().effect(Effect::Mica).build());
    }
}

/// Like a launcher: clicking anywhere else hides it, and it is destroyed once it has stayed
/// hidden for `SWITCHER_LINGER`. Closing it (Alt+F4) destroys it at once.
fn watch_switcher(win: &tauri::WebviewWindow) {
    #[cfg(windows)]
    dock::exclude_from_peek(win);
    let w = win.clone();
    win.on_window_event(move |e| {
        if let WindowEvent::Focused(false) = e {
            // Just shown, WebView2 takes the keyboard focus from the window, which reports a blur
            // while the switcher is still in front; hiding then made it flash and vanish. Hide only
            // once the foreground has really gone elsewhere.
            let w = w.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(SWITCHER_BLUR_SETTLE).await;
                if w.is_visible().unwrap_or(false) && !dock::is_foreground(&w) {
                    hide_switcher_window(&w);
                }
            });
        }
    });
}

/// Hides the switcher, then destroys it unless it was shown again in the meantime.
fn hide_switcher_window(w: &tauri::WebviewWindow) {
    let _ = w.hide();
    let w = w.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(SWITCHER_LINGER).await;
        if !w.is_visible().unwrap_or(true) {
            let _ = w.destroy();
        }
    });
}

fn show_switcher(app: &AppHandle, w: &tauri::WebviewWindow) {
    let _ = w.center();
    let _ = w.show();
    let _ = w.set_focus();
    let _ = app.emit_to(SWITCHER_LABEL, "switcher-open", ());
}

/// Shows the switcher in the middle of the screen, or hides it when it is open (the shortcut).
pub(crate) fn toggle_switcher(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(w) = on_demand_window(&app, SWITCHER_LABEL).await else { return };
        if w.is_visible().unwrap_or(false) { hide_switcher_window(&w); } else { show_switcher(&app, &w); }
    });
}

#[tauri::command]
async fn open_switcher(app: AppHandle) {
    // The switcher takes the focus while the flyout still holds the foreground; the flyout then
    // hides on its own blur. Hiding the flyout first handed the foreground back to the previous
    // app while the switcher was being built, and the switcher hid as soon as it appeared.
    if let Some(w) = on_demand_window(&app, SWITCHER_LABEL).await {
        if !w.is_visible().unwrap_or(false) { show_switcher(&app, &w); }
    }
    if let Some(panel) = app.get_webview_window(panel::LABEL) {
        if panel.is_visible().unwrap_or(false) { panel::close(&app); }
    }
}

#[tauri::command]
fn hide_switcher(app: AppHandle) {
    if let Some(w) = app.get_webview_window(SWITCHER_LABEL) {
        hide_switcher_window(&w);
    }
}

/// A settings shortcut ("Ctrl+Alt+S") as the plugin parses it.
fn parse_hotkey(hotkey: &str) -> Option<tauri_plugin_global_shortcut::Shortcut> {
    hotkey.replace("Win", "Super").parse().ok()
}

/// Registers the system-wide shortcuts from settings (the switcher's and the compact toggle's),
/// replacing the previous ones.
fn apply_hotkey(app: &AppHandle) {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let (switcher, compact) = {
        let state = app.state::<AppState>();
        let s = state.settings.lock().unwrap();
        (s.switcher_hotkey.clone(), s.compact_hotkey.clone())
    };
    let shortcuts = app.global_shortcut();
    let _ = shortcuts.unregister_all();
    let register = |hotkey: &str| if hotkey.is_empty() {
        None
    } else {
        match parse_hotkey(hotkey) {
            Some(shortcut) => shortcuts.register(shortcut).err().map(|_| "hotkey_taken".to_string()),
            None => Some("hotkey_invalid".to_string()),
        }
    };
    let state = app.state::<AppState>();
    *state.hotkey_error.lock().unwrap() = register(&switcher);
    *state.compact_hotkey_error.lock().unwrap() = register(&compact);
}

/// A pressed shortcut: the compact toggle's, or else the switcher's.
fn hotkey_pressed(app: &AppHandle, shortcut: &tauri_plugin_global_shortcut::Shortcut) {
    let compact = app.state::<AppState>().settings.lock().unwrap().compact_hotkey.clone();
    if !compact.is_empty() && parse_hotkey(&compact).as_ref() == Some(shortcut) {
        toggle_compact_flyout(app, true);
    } else {
        toggle_switcher(app);
    }
}

/// Why a shortcut is not working, if it is not: "hotkey_taken" or "hotkey_invalid". `which` is
/// "compact" for the compact toggle's; otherwise the switcher's.
#[tauri::command]
fn hotkey_status(state: tauri::State<AppState>, which: Option<String>) -> Option<String> {
    let error = if which.as_deref() == Some("compact") { &state.compact_hotkey_error } else { &state.hotkey_error };
    error.lock().unwrap().clone()
}

/// The flyout's own layout button: the flyout redraws in the new layout, so no toast.
#[tauri::command]
fn toggle_compact(app: AppHandle) {
    toggle_compact_flyout(&app, false);
}

/// Turns the compact flyout on or off. From the menus and the shortcut it is `announce`d in the
/// widget, since the flyout is usually closed.
pub(crate) fn toggle_compact_flyout(app: &AppHandle, announce: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let held = state.settings_lock.lock().await;
        let mut settings = state.settings.lock().unwrap().clone();
        settings.compact_flyout = !settings.compact_flyout;
        // Choosing a layout answers the flyout's offer of the compact one.
        settings.compact_suggested = true;
        let on = settings.compact_flyout;
        if let Ok(refresh) = store_settings(&app, held, settings) {
            if refresh {
                refresh_soon(&app);
            }
            if announce {
                let _ = app.emit("compact-toggled", on);
            }
        }
    });
}

#[derive(Serialize)]
struct Disconnected { snapshot: Snapshot, revoked: bool }

#[tauri::command]
async fn disconnect_monitoring(app: AppHandle, provider: Provider, id: String) -> Result<Disconnected, String> {
    let revoked = disconnect_one(&app, provider, &id).await?;
    Ok(Disconnected { snapshot: do_refresh(&app).await, revoked })
}

/// What "Revoke all SlopBar sessions" did for one account.
#[derive(Serialize)]
struct RevokedSessions {
    snapshot: Snapshot,
    /// "revoked" (the provider confirmed), "unconfirmed" (forgotten here, the provider did not
    /// confirm) or "none" (no monitoring sign-in was connected).
    monitoring: &'static str,
    slot_removed: bool,
    /// Always "managed_by_cli": the CLI's own sign-in is the CLI's to end (its logout).
    cli_login: &'static str,
}

/// Whether the account has monitoring connected, and whether it is in use.
fn session_facts(app: &AppHandle, provider: Provider, id: &str) -> Option<(bool, bool)> {
    let snapshot = app.state::<AppState>().snapshot.lock().unwrap().clone();
    snapshot.accounts.iter().find(|a| a.provider == provider && a.id == id).map(|a| (a.monitor_connected, a.active))
}

/// Revokes every sign-in SlopBar itself holds for an account: its monitoring session, at the
/// provider and here. The CLI's own login stays the CLI's; with `remove_slot` the saved copy of it
/// in the vault goes too (as Remove does), which needs the account not to be in use.
async fn revoke_sessions(app: &AppHandle, provider: Provider, id: &str, remove_slot: bool) -> Result<RevokedSessions, String> {
    let (connected, active) = session_facts(app, provider, id).ok_or("monitor_wrong_account")?;
    if remove_slot && active {
        return Err("active".into());
    }
    let monitoring = match (connected, disconnect_one(app, provider, id).await?) {
        (_, true) => "revoked",
        (true, false) => "unconfirmed",
        (false, false) => "none",
    };
    if remove_slot {
        remove_one(app, provider, id).await?;
    }
    Ok(RevokedSessions { snapshot: do_refresh(app).await, monitoring, slot_removed: remove_slot, cli_login: "managed_by_cli" })
}

#[tauri::command]
async fn revoke_account_sessions(app: AppHandle, provider: Provider, id: String, remove_slot: bool) -> Result<RevokedSessions, String> {
    revoke_sessions(&app, provider, &id, remove_slot).await
}

/// Revokes and forgets the account's monitoring sign-in; whether the provider confirmed it.
async fn disconnect_one(app: &AppHandle, provider: Provider, id: &str) -> Result<bool, String> {
    let id = id.to_string();
    let operation = app.state::<AppState>().account_operations.begin(provider, &id)?;
    let slot = accounts::load_slots().into_iter().find(|s| s.provider == provider && s.id == id)
        .ok_or("monitor_wrong_account")?;
    let started = Instant::now();
    let result = app.state::<AppState>().monitor.disconnect(&slot, &http_client()).await;
    app.state::<AppState>().diagnostics.record(Some(provider), Some(&id), diagnostics::Event::Action { action: diagnostics::Action::Disconnect,
        code: diagnostics::Code::from_error(result.as_ref().err().map(String::as_str)), elapsed_ms: diagnostics::millis(started) });
    let revoked = result?;
    auto_monitor_saved(app, |t| t.manual(&monitoring::key(&slot), false));
    // The account is free again before anything waits for a refresh; its backoff is cleared by the
    // next one, which the callers run.
    drop(operation);
    retry_soon(app, monitoring::key(&slot));
    Ok(revoked)
}

#[tauri::command]
async fn check_for_updates(app: AppHandle) -> Result<updates::UpdateInfo, String> {
    updates::check(&app).await
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    updates::install(&app).await
}

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    let version = app.package_info().version.to_string();
    if cfg!(feature = "upstream-updates") { version } else { format!("{version} (independent monitoring)") }
}

#[tauri::command]
fn updates_enabled() -> bool { cfg!(feature = "upstream-updates") }

/// The OS display language (WebView2's navigator.languages doesn't follow it reliably).
#[tauri::command]
fn system_locale() -> Option<String> {
    sys_locale::get_locale()
}

/// This computer's name, so the windows can say which machine an account is in use on and mark
/// this one among the others (`presence::this_machine`).
#[tauri::command]
fn this_machine() -> Option<String> {
    presence::this_machine()
}

/// Sync folders (OneDrive, Dropbox, iCloud Drive) Settings can offer for sharing in-use status.
#[tauri::command]
async fn sync_folders() -> Vec<presence::SyncFolder> {
    tauri::async_runtime::spawn_blocking(presence::sync_folders).await.unwrap_or_default()
}

#[derive(Serialize)]
struct RcloneRemotes { installed: bool, remotes: Vec<crate::rclone::Remote> }

/// The user's rclone remotes (names and types only), for sharing in-use status without a sync app.
#[tauri::command]
async fn rclone_remotes() -> RcloneRemotes {
    tauri::async_runtime::spawn_blocking(|| match crate::rclone::find() {
        Some(exe) => RcloneRemotes { installed: true, remotes: crate::rclone::remotes(&crate::rclone::Cli::local(exe)) },
        None => RcloneRemotes { installed: false, remotes: vec![] },
    }).await.unwrap_or(RcloneRemotes { installed: false, remotes: vec![] })
}

/// Whether a typed folder can hold presence files.
#[tauri::command]
fn check_presence_folder(folder: String) -> bool {
    settings::valid_presence_folder(folder.trim())
}

#[tauri::command]
fn get_settings(state: tauri::State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
async fn save_settings(app: AppHandle, settings: Settings) -> Result<(), String> {
    let state = app.state::<AppState>();
    let held = state.settings_lock.lock().await;
    if store_settings(&app, held, settings)? {
        do_refresh(&app).await;
    }
    Ok(())
}

/// Saves and applies settings, then lets go of `held`, the `settings_lock` its caller took before
/// reading the settings it changed. It waits on nothing (it is no async fn): what the change moved
/// (sharing elsewhere, the roster off) is withdrawn by the next refresh (see `sharing`). Returns
/// whether the change needs a refresh (the providers, where sharing goes, or the roster changed),
/// which the caller runs with the lock let go: a refresh that has to wait for the refresh lock
/// never keeps the next settings change waiting.
fn store_settings(app: &AppHandle, held: tokio::sync::MutexGuard<'_, ()>, settings: Settings) -> Result<bool, String> {
    // A changed shortcut is checked here too: the window saves whole settings.
    let (stored, stored_compact) = {
        let state = app.state::<AppState>();
        let s = state.settings.lock().unwrap();
        (s.switcher_hotkey.clone(), s.compact_hotkey.clone())
    };
    let settings = if settings.switcher_hotkey == stored { settings } else {
        Settings { switcher_hotkey: settings::normalize_hotkey(&settings.switcher_hotkey)?, ..settings }
    };
    let settings = if settings.compact_hotkey == stored_compact { settings } else {
        Settings { compact_hotkey: settings::normalize_hotkey(&settings.compact_hotkey)?, ..settings }
    };
    settings::distinct_hotkeys(&settings)?;
    // And a changed phone-alert URL: only an ntfy topic URL, never plain http to the internet.
    let stored_phone = app.state::<AppState>().settings.lock().unwrap().phone.url.clone();
    let mut settings = settings;
    if settings.phone.url != stored_phone {
        if let Some(url) = settings.phone.url.as_deref().filter(|u| !u.trim().is_empty()) {
            settings.phone.url = Some(phone::normalize_url(url)?);
        } else {
            settings.phone.url = None;
        }
    }
    if settings.phone.url.is_none() { settings.phone.enabled = false; }
    settings::save(&settings)?;
    diagnostics::set_crash_records(settings.crash_records);
    let previous = std::mem::replace(&mut *app.state::<AppState>().settings.lock().unwrap(), settings.clone());
    apply_settings(app);
    if previous.switcher_hotkey != settings.switcher_hotkey || previous.compact_hotkey != settings.compact_hotkey {
        apply_hotkey(app);
    }
    let state = app.state::<AppState>();
    if previous.presence_folder != settings.presence_folder || previous.presence_remote != settings.presence_remote {
        state.health.lock().unwrap().sharing = None;
    }
    // The old place's machines should not keep seeing this one as signed in, nor keep its accounts:
    // the next refresh withdraws them there, after any write made meanwhile for that place.
    let refresh = state.sharing.saved(&previous, &settings);
    drop(held);
    Ok(refresh)
}

/// Shows Settings, on `page` when given (the flyout's health badge opens Health).
#[tauri::command]
fn open_settings(app: AppHandle, page: Option<String>) {
    show_settings_page(&app, page);
}

/// The page Settings was asked to open on while it was being created, once.
#[tauri::command]
fn take_settings_page(state: tauri::State<AppState>) -> Option<String> {
    state.settings_page.lock().unwrap().take()
}

#[tauri::command]
fn set_panel_height(app: AppHandle, height: f64) {
    *app.state::<AppState>().panel_height.lock().unwrap() = height;
    panel::page_ready();
    panel::place(&app);
}

#[tauri::command]
fn set_dock_width(app: AppHandle, width: f64) {
    *app.state::<AppState>().dock_width.lock().unwrap() = width;
}

/// A click on the taskbar widget (web or native) opens the flyout above it.
#[tauri::command]
pub(crate) fn dock_clicked(app: AppHandle) {
    if let Some(anchor) = dock::anchor(&app) {
        *app.state::<AppState>().panel_anchor.lock().unwrap() = Some(anchor);
    }
    panel::toggle(&app);
}

/// The Windows accent color as WinUI uses it on light and dark backgrounds
/// (SystemAccentColorDark1 and SystemAccentColorLight2), from the system's accent palette.
#[tauri::command]
fn accent_colors() -> Option<(String, String)> {
    let p = dock::accent_palette()?;
    let hex = |i: usize| format!("#{:02x}{:02x}{:02x}", p[i * 4], p[i * 4 + 1], p[i * 4 + 2]);
    // Palette entries: Light3, Light2, Light1, Accent, Dark1, Dark2, Dark3, (unused).
    Some((hex(4), hex(1)))
}

/// Whether the settings window is on Mica (Windows 11); elsewhere it paints its own background.
#[tauri::command]
fn has_mica() -> bool {
    dock::is_windows_11()
}

#[tauri::command]
fn taskbar_theme() -> &'static str {
    if dock::taskbar_is_light() { "light" } else { "dark" }
}

/// Right-click on the widget: the native context menu, like the built-in tray items have.
#[tauri::command]
fn dock_menu(app: AppHandle) {
    let menu = app.state::<AppState>().menu.lock().unwrap().clone();
    if let (Some(menu), Some(w)) = (menu, app.get_webview_window(dock::LABEL)) {
        let _ = w.popup_menu(&menu);
    }
}

/// Menu labels come from the frontend, which owns the translations.
#[tauri::command]
fn set_menu_labels(state: tauri::State<AppState>, labels: HashMap<String, String>) {
    for item in state.menu_items.lock().unwrap().iter() {
        if let Some(text) = labels.get(item.id().as_ref()) {
            let _ = item.set_text(text);
        }
    }
    if let (Some(item), Some(text)) = (state.compact_item.lock().unwrap().as_ref(), labels.get(MENU_COMPACT)) {
        let _ = item.set_text(text);
    }
}

pub(crate) fn show_settings(app: &AppHandle) {
    show_settings_page(app, None);
}

/// Shows Settings, on `page` if it is one of its pages, creating the window if it is closed.
pub(crate) fn show_settings_page(app: &AppHandle, page: Option<String>) {
    let page = page.filter(|p| door::SETTINGS_PAGES.contains(&p.as_str()));
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let w = match app.get_webview_window(SETTINGS_LABEL) {
            Some(w) => {
                if let Some(page) = &page {
                    let _ = app.emit_to(SETTINGS_LABEL, "show-page", page);
                }
                w
            }
            None => {
                // The new page reads it when it loads (`take_settings_page`).
                *app.state::<AppState>().settings_page.lock().unwrap() = page;
                let Some(w) = on_demand_window(&app, SETTINGS_LABEL).await else { return };
                w
            }
        };
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    });
}

/// The widget's right-click menu (the web widget's Tauri menu, or the native widget's own).
pub(crate) fn menu_event(app: &AppHandle, id: &str) {
    match id {
        "refresh" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move { do_refresh(&app).await });
        }
        "settings" => show_settings(app),
        MENU_COMPACT => toggle_compact_flyout(app, true),
        "quit" => app.exit(0),
        _ => {}
    }
}

/// Applies the settings that live outside the webviews.
pub(crate) fn apply_settings(app: &AppHandle) {
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    let autolaunch = app.autolaunch();
    if !settings::isolated() && autolaunch.is_enabled().unwrap_or(false) != settings.launch_at_login {
        let _ = if settings.launch_at_login { autolaunch.enable() } else { autolaunch.disable() };
    }
    dock::apply_mode(app, settings.native_widget);
    sources::set_wsl_mode(settings.wsl_accounts);
    if let Some(item) = app.state::<AppState>().compact_item.lock().unwrap().as_ref() {
        let _ = item.set_checked(settings.compact_flyout);
    }
    let _ = app.emit("settings-changed", &settings);
}

/// The client for the providers' usage and sign-in APIs: SlopBar's own (`net::builder`: Windows'
/// certificate checks, the system's proxy), following no redirect.
pub(crate) fn http_client() -> reqwest::Client {
    crate::net::builder()
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client")
}

/// A refresh (`refresh_within`), or the last numbers when it gave up waiting for the refresh lock.
async fn do_refresh(app: &AppHandle) -> Snapshot {
    match refresh_within(app).await {
        Some(snapshot) => snapshot,
        None => app.state::<AppState>().snapshot.lock().unwrap().clone(),
    }
}

/// A refresh, once the refresh lock is free: it waits `refresh_wait::LOCK_WAIT` at most for a
/// refresh running long, or, in builds that wait for a refresh before an update's installer, for
/// that update's handover, which keeps the lock (`update_check::hand_over`). Then it gives up
/// (None), the door call it serves says so (`refresh_wait::noting_waits`), and a refresh follows
/// as soon as the lock is free (`refresh_soon`).
async fn refresh_within(app: &AppHandle) -> Option<Snapshot> {
    let state = app.state::<AppState>();
    let refreshed = refresh_wait::under(&state.refresh_lock, refresh_wait::LOCK_WAIT, || refresh_locked(app)).await;
    if refreshed.is_none() {
        refresh_soon(app);
    }
    refreshed
}

/// Captures live logins into the vault, fetches every account's usage, then lets the balancer
/// switch accounts that reached their threshold. The widget gets the new numbers as soon as they
/// are known; sharing and bookkeeping follow, and the index, calendar and Codex scans run after.
/// Runs under the refresh lock (`refresh_within`).
async fn refresh_locked(app: &AppHandle) -> Snapshot {
    use diagnostics::RefreshPhase as P;
    let state = app.state::<AppState>();
    // Changes to the cache left for this refresh (backoffs cleared, removed accounts dropped):
    // under the lock, so none is written back by a refresh that read the cache before.
    state.cache_due.drain_into(&mut state.usage_cache.lock().unwrap());
    // What settings saves left (`sharing`): this machine's presence and roster withdrawn from the
    // places sharing left, before the settings are read and anything is written where they point.
    // A place that does not answer holds the refresh up for WITHDRAW_LIMIT at most; what it still
    // has stays due.
    if state.sharing.is_due() {
        let interval = state.settings.lock().unwrap().refresh_minutes.max(1) as u64 * 60;
        let sharing = state.sharing.clone();
        let reset = tauri::async_runtime::spawn_blocking(move || sharing.catch_up(&sharing::Files { interval }, sharing::WITHDRAW_LIMIT)).await.unwrap_or(false);
        if reset {
            *state.roster_seen.lock().unwrap() = roster::Seen::default();
        }
    }
    // Ended leases free their accounts (and profiles) before this refresh decides anything.
    state.leases.lock().unwrap().prune(chrono::Utc::now(), leases::process_alive);
    let started = Instant::now();
    let mut phases = diagnostics::Phases::start();
    let settings = state.settings.lock().unwrap().clone();
    state.diagnostics.poll_started(settings.refresh_minutes);
    let found = tauri::async_runtime::spawn_blocking(sources::discover)
        .await
        .unwrap_or_default();
    *state.sources.lock().unwrap() = found.clone();
    phases.end(P::Discover);
    let profiles = accounts::profile_sources(&profile_accounts(&state));
    accounts::resolve_identities(&http_client(), &[found.clone(), profiles].concat()).await;
    phases.end(P::Identities);
    let scan = found.clone();
    let active = tauri::async_runtime::spawn_blocking(move || accounts::capture(&scan))
        .await
        .unwrap_or_default();
    phases.end(P::Capture);
    // Before reading the vault: renewals made in project profiles are synced into it.
    sync_pins(app, Some(active.clone())).await;
    phases.end(P::Pins);
    let slots = tauri::async_runtime::spawn_blocking(accounts::load_slots).await.unwrap_or_default();
    phases.end(P::Vault);
    static CLEANUP_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !CLEANUP_STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        // Leftover sign-in folders from earlier runs: once, beside this refresh.
        let (app, homes, slots) = (app.clone(), found.clone(), slots.clone());
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            let protected = state.monitor.protected_tokens(&slots).await;
            let started = Instant::now();
            let result = tauri::async_runtime::spawn_blocking(move || crate::login::cleanup(&homes, protected.as_ref())).await;
            state.diagnostics.record(None, None, diagnostics::Event::Action { action: diagnostics::Action::Cleanup,
                code: if result.is_ok() { diagnostics::Code::Ok } else { diagnostics::Code::Other }, elapsed_ms: diagnostics::millis(started) });
        });
    }

    let mut cache = state.usage_cache.lock().unwrap().clone();
    let mut list = providers::fetch_all(&http_client(), &slots, &active, &mut cache, &settings, &state.monitor).await;
    let valid: HashSet<_> = slots.iter().map(monitoring::key).collect();
    cache.retain(|key, _| valid.contains(key));
    *state.usage_cache.lock().unwrap() = cache.clone();
    phases.end(P::Fetch);

    // What the last refresh worked out, for the early redraw: before a switch below publishes this
    // refresh's bare numbers.
    let previous = state.snapshot.lock().unwrap().accounts.clone();
    let decisions = {
        let last = state.last_switch.lock().unwrap().clone();
        let leased = state.leases.lock().unwrap().accounts();
        balancer::decide_avoiding(&list, &settings, &last, &leased)
    };
    let decisions = graceful_decisions(app, &list, &settings, decisions);
    for (provider, id) in decisions {
        // Publish the current numbers first so the switch notice can name both accounts, with what
        // sharing and the rosters last read add to them, as the early redraw below has.
        let mut accounts = list.clone();
        carry_over(&mut accounts, &previous);
        apply_shared(&state, &settings, &mut accounts);
        *state.snapshot.lock().unwrap() = Snapshot { accounts, updated_at: Some(chrono::Utc::now()) };
        if switch_to(app, provider, id.clone(), true).await.is_ok() {
            state.last_switch.lock().unwrap().insert(provider, Instant::now());
            for a in list.iter_mut().filter(|a| a.provider == provider) {
                let now_active = a.id == id;
                if now_active != a.active {
                    a.sources = if now_active {
                        found.iter().map(|s| s.label.clone()).collect()
                    } else {
                        vec![]
                    };
                    a.active = now_active;
                    // Never for the account in use; again for the one switched away from.
                    a.cli_renewal = accounts::cli_renewal(a.cli_ends_at, now_active, chrono::Utc::now());
                }
            }
        }
    }
    phases.end(P::Balance);
    // Beside the CLI: which account each desktop app here is signed in to.
    let apps = tauri::async_runtime::spawn_blocking(crate::desktop_apps::read).await.unwrap_or_default();
    crate::desktop_apps::apply(&mut list, &apps);

    // The widget redraws now: what sharing and forecasts add is carried over from the last
    // refresh until they are worked out below.
    {
        let mut early = list.clone();
        carry_over(&mut early, &previous);
        // Other machines' readings as the rosters last read have them, against this refresh's own.
        apply_shared(&state, &settings, &mut early);
        let early = Snapshot { accounts: early, updated_at: Some(chrono::Utc::now()) };
        *state.snapshot.lock().unwrap() = early.clone();
        let _ = app.emit("usage-updated", &early);
        crate::alerts::on_early_snapshot(&app);
    }
    phases.end(P::Publish);
    if providers::save_cache(&cache).is_err() {
        state.diagnostics.record(None, None, diagnostics::Event::Action { action: diagnostics::Action::CacheSave, code: diagnostics::Code::Storage, elapsed_ms: 0 });
    }

    // Other machines: what they share through the presence folder, then what rising usage implies.
    let now = chrono::Utc::now();
    let interval = settings.refresh_minutes.max(1) as u64 * 60;
    // Whether every computer is covered: sharing is on and someone (this one or a peer) published.
    let mut covered = false;
    let mut shared = None;
    if let Some(store) = presence::store(&settings) {
        let mine = list.clone();
        let (place, h) = (sharing::Place::of(&settings), app.clone());
        let read_from = place.clone();
        let exchanged = tauri::async_runtime::spawn_blocking(move || {
            let Some((id, name)) = presence::identity() else { return Some((vec![], health::SharingResult::failed(now))) };
            // A save that moved sharing since this refresh read the settings: nothing is written to
            // the place it left, which the next refresh withdraws from (see `sharing`).
            if !sharing::still(&place, &sharing::Place::of(&h.state::<AppState>().settings.lock().unwrap())) {
                return None;
            }
            let wrote = store.write(&presence::own(id, name, interval, &mine, now));
            let listed = store.list();
            // With the network's proxy's code for whichever it stopped, which Health words.
            let result = health::SharingResult::of(now, &wrote, &listed);
            Some((listed.unwrap_or_default(), result))
        }).await.unwrap_or(Some((vec![], health::SharingResult::failed(now))));
        if let Some((files, result)) = exchanged {
            let (own, me) = presence::identity().map_or(("", ""), |(id, name)| (id.as_str(), name.as_str()));
            // What the read found raises alarms; what accounts show also keeps a record the read
            // missed, while that record is live (`presence::Recall`).
            let found = presence::peers(files.iter().cloned(), own, now);
            let every = presence::peers(state.presence_recall.lock().unwrap().fill(&read_from, result.read.then_some(files.as_slice()), own, now), own, now);
            shared = Some(result);
            covered = result.wrote || found.iter().any(|p| p.live);
            presence::apply(&mut list, &found, &every, me, now);
        }
    }
    // Two refresh intervals without a rise (readings are at least a minute apart), plus slack.
    let hold = chrono::Duration::seconds(2 * interval.max(60) as i64 + 30);
    // Consecutive readings of a few refreshes at most (a missed one is fine; a night asleep is not).
    let surge = presence::Surge {
        min_rise: settings.unexpected_min_rise.max(1) as f64,
        max_gap: chrono::Duration::seconds((3 * interval as i64 + 60).max(15 * 60)),
    };
    let surged = presence::infer(&mut state.presence_tracks.lock().unwrap(), &mut list, hold, surge, now);
    if settings.account_roster {
        sync_roster(app, &settings, list.clone(), false, false).await;
        apply_shared(&state, &settings, &mut list);
    }
    let watch = unexpected::Watch { covered, notify: settings.notifications.unexpected_use, web_use: &settings.web_use };
    let log = {
        let mut log = state.unexpected_log.lock().unwrap();
        unexpected::apply(&mut log, &mut list, &surged, &watch, now).then(|| log.clone())
    };
    if let Some(log) = log {
        let _ = unexpected::save(&log);
    }
    phases.end(P::Presence);

    let mut snapshot = Snapshot { accounts: list, updated_at: Some(chrono::Utc::now()) };
    state.diagnostics.samples(&snapshot);
    note_active(&state, &snapshot, settings.refresh_minutes);
    let history = {
        let mut history = state.usage_history.lock().unwrap();
        subscriptions::record(&mut history, &snapshot, &valid, chrono::Utc::now());
        history.clone()
    };
    {
        let mut recent = state.recent.lock().unwrap();
        let now = chrono::Utc::now();
        recent.record(&snapshot.accounts, now);
        headroom::attach_forecasts(&mut snapshot, &settings, &recent, now);
    }
    if subscriptions::save(&history).is_err() { state.diagnostics.record(None, None, diagnostics::Event::Action { action: diagnostics::Action::HistorySave, code: diagnostics::Code::Storage, elapsed_ms: 0 }); }
    state.diagnostics.record(None, None, diagnostics::Event::PollFinished { elapsed_ms: diagnostics::millis(started), accounts: snapshot.accounts.len(),
        native_sources: found.iter().filter(|s| s.distro.is_none()).count(), wsl_sources: found.iter().filter(|s| s.distro.is_some()).count() });
    {
        let mut seen = state.health.lock().unwrap();
        seen.sharing = shared;
        if snapshot.accounts.is_empty() || snapshot.accounts.iter().any(|a| !a.stale) {
            seen.last_success = snapshot.updated_at;
        }
    }
    mark_leases(&state, &mut snapshot);
    phases.end(P::Bookkeeping);
    *state.snapshot.lock().unwrap() = snapshot.clone();
    record_spend(&state);
    let _ = app.emit("usage-updated", &snapshot);
    crate::alerts::on_snapshot(&app);
    phases.end(P::Final);
    state.diagnostics.record(None, None, phases.event());
    // Off the refresh: each runs one at a time on its own.
    if settings.providers.codex { scan_codex_local(app); }
    if settings.calendar.enabled {
        let app = app.clone();
        tauri::async_runtime::spawn(async move { let _ = write_calendar(&app).await; });
    }
    scan_usage_index(app);
    snapshot
}

/// Gives freshly fetched accounts what the last refresh worked out from sharing and forecasts,
/// so the early redraw does not blank them for a moment.
fn carry_over(list: &mut [crate::model::AccountUsage], previous: &[crate::model::AccountUsage]) {
    for a in list.iter_mut() {
        if let Some(p) = previous.iter().find(|p| p.provider == a.provider && p.id == a.id) {
            a.remote = p.remote.clone();
            a.used_outside = p.used_outside;
            a.runs_out_at = p.runs_out_at;
            a.unexpected = p.unexpected.clone();
        }
    }
    // A switch may have changed which account is in use here since the last refresh.
    presence::overlap(list, presence::identity().map_or("", |(_, name)| name.as_str()));
}

/// Other machines' readings of these accounts, from the rosters last read (no I/O): shown where
/// this machine has none fresh, and naming the machine whose recent reading covers each (see
/// `roster::apply_readings`). Only while the roster is on.
fn apply_shared(state: &AppState, settings: &settings::Settings, list: &mut [model::AccountUsage]) {
    if !settings.account_roster {
        return;
    }
    let me = presence::identity().map_or("", |(_, name)| name.as_str());
    // A refresh: another machine's reading must be newer than this one's own by more than that.
    let margin = chrono::Duration::minutes(settings.refresh_minutes.max(1) as i64);
    roster::apply_readings(list, &state.roster_seen.lock().unwrap(), me, margin, chrono::Utc::now());
}

/// Records which account each CLI has signed in, seeding the log once from the diagnostics
/// journal's past week. Saved when an interval starts, else every ten minutes.
fn note_active(state: &AppState, snapshot: &Snapshot, refresh_minutes: u32) {
    let now = chrono::Utc::now();
    let gap = active_log::max_gap(refresh_minutes);
    let marks = (!state.active_log.lock().unwrap().backfilled).then(|| state.diagnostics.active_marks());
    let mut log = state.active_log.lock().unwrap();
    let mut started = false;
    if let Some(marks) = marks {
        // The journal keeps a steady reading only hourly.
        log.backfill(marks, chrono::Duration::minutes(75));
        started = true;
    }
    for (provider, id) in active_log::active_ids(&snapshot.accounts) {
        started |= log.note(provider, id, now, gap);
    }
    let mut saved = state.active_saved.lock().unwrap();
    if started || saved.map_or(true, |t| t.elapsed() >= Duration::from_secs(600)) {
        log.prune(now);
        let _ = active_log::save(&log);
        *saved = Some(Instant::now());
    }
}

/// Re-reads the CLIs' transcripts and rollouts off the async runtime, one scan at a time. The
/// first run reads weeks of files; later ones only what changed.
fn scan_usage_index(app: &AppHandle) {
    static SCANNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if SCANNING.swap(true, std::sync::atomic::Ordering::AcqRel) { return; }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let index = tauri::async_runtime::spawn_blocking(|| usage_index::scan(chrono::Utc::now())).await;
        SCANNING.store(false, std::sync::atomic::Ordering::Release);
        let Ok(index) = index else { return };
        *app.state::<AppState>().usage_index.lock().unwrap() = Some(index.clone());
        let _ = tauri::async_runtime::spawn_blocking(move || check_burn(&app, &index)).await;
    });
}

/// Looks for runaway agents (see `burn`) with the index just read, queuing their notifications
/// for the taskbar widget.
fn check_burn(app: &AppHandle, index: &usage_index::Index) {
    let state = app.state::<AppState>();
    let snapshot = state.snapshot.lock().unwrap().clone();
    if snapshot.updated_at.is_none() { return; }
    let settings = state.settings.lock().unwrap().clone();
    let now = chrono::Utc::now();
    let log = state.active_log.lock().unwrap().clone();
    let window = chrono::Duration::minutes(settings.burn.minutes as i64);
    let rates = usage_index::local_rates(index, &log, &usage_index::Known::from_snapshot(&snapshot), window, chrono::Duration::days(burn::BASELINE_DAYS), now);
    let found = burn::detect(&snapshot.accounts, &state.recent.lock().unwrap(), &rates, &settings, now);
    let (due, changed) = {
        let mut log = state.burn_log.lock().unwrap();
        let (due, changed) = burn::apply(&mut log, found, settings.notifications.burn, now);
        (due, changed.then(|| log.clone()))
    };
    if let Some(log) = changed { let _ = burn::save(&log); }
    state.burn_due.lock().unwrap().extend(due);
}

/// The last index, or a scan now when there is none yet.
async fn usage_index_now(app: &AppHandle) -> std::sync::Arc<usage_index::Index> {
    if let Some(index) = app.state::<AppState>().usage_index.lock().unwrap().clone() { return index; }
    let index = tauri::async_runtime::spawn_blocking(|| usage_index::scan(chrono::Utc::now())).await
        .unwrap_or_else(|_| std::sync::Arc::new(usage_index::Index::default()));
    *app.state::<AppState>().usage_index.lock().unwrap() = Some(index.clone());
    index
}

async fn usage_breakdown(app: &AppHandle, filter: usage_index::Filter) -> usage_index::Breakdown {
    let index = usage_index_now(app).await;
    let state = app.state::<AppState>();
    let known = usage_index::Known::from_snapshot(&state.snapshot.lock().unwrap());
    let log = state.active_log.lock().unwrap().clone();
    usage_index::breakdown(&index, &log, &known, &filter, chrono::Utc::now(), &chrono::Local)
}

#[tauri::command]
async fn get_usage_breakdown(app: AppHandle, provider: Option<Provider>, account: Option<String>, days: Option<u32>, group_by: Option<usage_index::GroupBy>) -> usage_index::Breakdown {
    usage_breakdown(&app, usage_index::Filter { provider, account, days: days.unwrap_or(7), group_by: group_by.unwrap_or_default() }).await
}

/// Writes the CSV exports into `folder` (default Documents\SlopBar Exports).
async fn write_csv(app: &AppHandle, folder: Option<String>) -> Result<csv_export::Written, String> {
    let folder = match folder.map(|f| f.trim().to_string()).filter(|f| !f.is_empty()) {
        Some(f) => std::path::PathBuf::from(f),
        None => csv_export::default_folder().ok_or("no_documents_folder")?,
    };
    let index = usage_index_now(app).await;
    let state = app.state::<AppState>();
    let snapshot = state.snapshot.lock().unwrap().clone();
    let known = usage_index::Known::from_snapshot(&snapshot);
    let log = state.active_log.lock().unwrap().clone();
    let history = state.usage_history.lock().unwrap().clone();
    let report = subscription_report(&state);
    let ledger = state.ledger.lock().unwrap().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let now = chrono::Utc::now();
        let rows = usage_index::daily(&index, &log, &known, now, &chrono::Local);
        let stamp = now.with_timezone(&chrono::Local).format("%Y-%m-%d-%H%M%S").to_string();
        csv_export::write(&folder, &csv_export::history_csv(&history, &known), &csv_export::breakdown_csv(&rows), &csv_export::subscriptions_csv(&report), &csv_export::ledger_csv(&ledger), &stamp)
    }).await.map_err(|_| "export_folder_unwritable".to_string())?
}

#[tauri::command]
async fn export_csv(app: AppHandle, folder: Option<String>) -> Result<csv_export::Written, String> {
    let written = write_csv(&app, folder).await?;
    #[cfg(windows)]
    {
        let folder = written.folder.clone();
        let _ = tauri::async_runtime::spawn_blocking(move || std::process::Command::new("explorer.exe").arg(folder).spawn()).await;
    }
    Ok(written)
}

#[tauri::command]
fn csv_default_folder() -> Option<String> {
    csv_export::default_folder().map(|p| p.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------------------------
// Backup and restore (see `backup`)

/// Seals settings, the weekly history and the spend ledger into a backup file. Only the slots'
/// provider, id and email are read from the vault.
async fn write_backup(app: &AppHandle, path: Option<String>, passphrase: backup::Passphrase) -> Result<serde_json::Value, String> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let history = state.usage_history.lock().unwrap().clone();
    let ledger = state.ledger.lock().unwrap().clone();
    let version = app_version(app.clone());
    tauri::async_runtime::spawn_blocking(move || {
        let accounts = accounts::load_slots().into_iter().map(|s| backup::Account { provider: s.provider, id: s.id, email: s.email });
        let now = chrono::Utc::now();
        let contents = backup::Contents::new(&settings, &history, &ledger, accounts, &version, now);
        let stamp = now.with_timezone(&chrono::Local).format("%Y-%m-%d-%H%M%S").to_string();
        let target = backup::target(path.as_deref(), csv_export::default_folder(), &stamp)?;
        let bytes = backup::seal(&contents, &passphrase)?;
        backup::write(&target, &bytes)?;
        Ok(serde_json::json!({ "path": target, "bytes": bytes.len(), "note": backup::NOTE }))
    }).await.map_err(|_| "backup_unwritable".to_string())?
}

/// Opens a backup and previews merging it; unless `dry_run`, also saves and applies the result,
/// then refreshes once.
async fn apply_backup(app: &AppHandle, bytes: Vec<u8>, passphrase: backup::Passphrase, dry_run: bool) -> Result<backup::Preview, String> {
    let contents = tauri::async_runtime::spawn_blocking(move || backup::open(&bytes, &passphrase))
        .await.map_err(|_| "backup_unreadable".to_string())??;
    let known: Vec<(Provider, String)> = tauri::async_runtime::spawn_blocking(|| accounts::load_slots().into_iter().map(|s| (s.provider, s.id)).collect())
        .await.unwrap_or_default();
    let state = app.state::<AppState>();
    let held = state.settings_lock.lock().await;
    let current = state.settings.lock().unwrap().clone();
    let now = chrono::Utc::now();
    let (merged_settings, preview, history, ledger) = {
        let folder = |f: &str| pins::restored_folder(f);
        let presence_ok = |f: &str| settings::valid_presence_folder(f);
        let calendar_ok = |p: &str| calendar::valid_path(p);
        let here = backup::Here { known: &known, updates_enabled: updates_enabled(), folder: &folder, presence_folder_ok: &presence_ok, calendar_path_ok: &calendar_ok };
        // Both held while merging, so a refresh finishing meanwhile is not lost.
        let mut history = state.usage_history.lock().unwrap();
        let mut ledger = state.ledger.lock().unwrap();
        let (merged, preview) = backup::merge(&current, &history, &ledger, &contents, &here, now);
        if dry_run { return Ok(preview); }
        *history = merged.history.clone();
        *ledger = merged.ledger.clone();
        (merged.settings, preview, merged.history, merged.ledger)
    };
    subscriptions::save(&history)?;
    ledger::save(&ledger)?;
    // Lets go of `settings_lock`. A refresh the change needs is the one this ends with.
    store_settings(app, held, merged_settings)?;
    // Accounts to sign in again here: "Sign in all accounts" walks through them by email.
    let mut pending = bulk::load_pending();
    bulk::add_pending(&mut pending, preview.accounts.iter().filter(|a| !a.connected)
        .map(|a| bulk::Pending { provider: a.provider, id: a.id.clone(), email: a.email.clone() }));
    let _ = bulk::save_pending(&pending);
    sync_pins(app, None).await;
    do_refresh(app).await;
    Ok(preview)
}

#[tauri::command]
async fn export_backup(app: AppHandle, path: Option<String>, passphrase: backup::Passphrase) -> Result<serde_json::Value, String> {
    write_backup(&app, path, passphrase).await
}

/// The Settings window reads the chosen file itself and passes its contents.
#[tauri::command]
async fn restore_backup(app: AppHandle, contents: String, passphrase: backup::Passphrase, dry_run: bool) -> Result<backup::Preview, String> {
    apply_backup(&app, contents.into_bytes(), passphrase, dry_run).await
}

// ---------------------------------------------------------------------------------------------
// Phone alerts (see `phone`)

#[tauri::command]
fn phone_token_saved() -> bool {
    settings::app_dir().is_some_and(|d| phone::load_token(&d).is_some())
}

/// Saves the access token with DPAPI; null or empty deletes it.
#[tauri::command]
async fn set_phone_token(token: Option<String>) -> Result<(), String> {
    let dir = settings::app_dir().ok_or("phone_token_storage")?;
    tauri::async_runtime::spawn_blocking(move || phone::store_token(&dir, token.as_deref())).await.map_err(|_| "phone_token_storage".to_string())?
}

// ---------------------------------------------------------------------------------------------
// Sharing setup (see `sharing_setup`)

/// Changes the setup's state and tells the windows.
fn publish_sharing(app: &AppHandle, change: impl FnOnce(&mut sharing_setup::Setup)) -> sharing_setup::Setup {
    let state = app.state::<AppState>();
    let out = {
        let mut setup = state.sharing_setup.lock().unwrap();
        change(&mut setup);
        setup.revision += 1;
        setup.clone()
    };
    let _ = app.emit("sharing-setup", &out);
    out
}

/// Whether a sharing setup is under way here, or this computer holds the tunnel for another
/// computer's sign-in: an automatic update waits for either (see `updates`), since restarting would
/// end the Dropbox sign-in. A tunnel closes within `SIGN_IN_TIMEOUT` and two minutes at the latest.
pub(crate) fn sharing_setup_running(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let here = state.sharing_setup.lock().unwrap().phase.running();
    here || state.sharing_tunnel.lock().unwrap().is_some()
}

/// Opens one of rclone's loopback sign-in links in the default browser through url.dll's protocol
/// handler, as rclone itself does: explorer.exe takes a link with a query string for a folder and
/// opens Documents instead. Windows lets a window come to the front only for the app the user is
/// using, so whatever right this one has is passed on.
fn open_in_browser(link: &str) -> Result<(), String> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};
        AllowSetForegroundWindow(ASFW_ANY);
    }
    let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
    std::process::Command::new(system.join(r"System32\rundll32.exe")).arg("url.dll,FileProtocolHandler").arg(link)
        .spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Opens Dropbox's page (unless the user turned that off: `sharing_open_browser`) and says so in a
/// notification whose click opens it (unless turned off: `notifications.sharing_sign_in`): a page
/// opened while SlopBar works in the background can stay behind other windows, and the click lets
/// the browser come to the front. `machine` names the computer the sign-in is for, when it is
/// another one. Answers whether the page opened and whether the notification showed.
fn offer_sharing_page(app: &AppHandle, link: String, machine: Option<&str>) -> (bool, bool) {
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    let opened = settings.sharing_open_browser && open_in_browser(&link).is_ok();
    if !settings.notifications.sharing_sign_in {
        return (opened, false);
    }
    let lang = crate::i18n::resolve(&settings.language);
    let body = match machine {
        Some(machine) => crate::i18n::t(&lang, "sharingStepDropboxFor", &[("machine", machine)]),
        None => crate::i18n::t(&lang, "sharingStepDropboxBusy", &[]),
    };
    let buttons = vec![(crate::i18n::t(&lang, "sharingStepDropboxAgain", &[]), "open")];
    let notified = notify::notify_choice(app.clone(), crate::i18n::t(&lang, "sharingStepDropbox", &[]), body, buttons, move |_| {
        let _ = open_in_browser(&link);
    }).is_ok();
    (opened, notified)
}

/// Starts a setup and answers at once with its state. Dropbox's page opens on this screen, so it
/// must be shown, unless the sign-in is finished on another computer (`via_tunnel`). The remote is
/// the one named, else the one sharing uses already, else "dropbox". `reconnect` signs in again
/// even when the remote works (another Dropbox account).
fn start_sharing_setup(app: &AppHandle, remote: Option<String>, via_tunnel: bool, reconnect: bool) -> Result<sharing_setup::Setup, String> {
    use sharing_setup::{Phase, Setup};
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    // A computer sharing through a folder keeps it: the setup would move it off that folder.
    if settings.presence_folder.is_some() {
        return Err("uses_folder".into());
    }
    let remote = remote.or(settings.presence_remote).unwrap_or_else(|| sharing_setup::DEFAULT_REMOTE.into());
    if !rclone::valid_remote(&remote) {
        return Err("bad_remote".into());
    }
    if !via_tunnel {
        desktop_shown()?;
    }
    let started = {
        let mut setup = state.sharing_setup.lock().unwrap();
        if setup.phase.running() {
            return Err("sharing_setup_busy".into());
        }
        *setup = Setup { phase: Phase::Rclone, remote: remote.clone(), via_tunnel, revision: setup.revision + 1, ..Default::default() };
        setup.clone()
    };
    state.sharing_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    let _ = app.emit("sharing-setup", &started);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // In a task of its own, so that a panic inside ends as a failed setup too, as does any exit
        // that leaves it under way: a phase left running would hold the automatic update back
        // until SlopBar restarts.
        let run = {
            let app = app.clone();
            tauri::async_runtime::spawn(async move { run_sharing_setup(&app, remote, via_tunnel, reconnect).await })
        };
        let outcome = run.await.unwrap_or(Err("setup_failed"));
        let stuck = app.state::<AppState>().sharing_setup.lock().unwrap().phase.running();
        if let Err(code) = outcome.and_then(|()| if stuck { Err("setup_failed") } else { Ok(()) }) {
            publish_sharing(&app, |s| {
                s.failed_at = Some(s.phase);
                s.phase = Phase::Failed;
                s.error = Some(code.into());
                s.waiting = false;
                s.link = None;
                s.progress = None;
            });
        }
    });
    Ok(started)
}

/// The setup's steps: rclone (SlopBar's own verified copy when none recent enough is installed),
/// the remote (created, signed in again when its access ended or another account is wanted, or
/// used as it is), sharing on, and who else shares there.
async fn run_sharing_setup(app: &AppHandle, remote: String, via_tunnel: bool, reconnect: bool) -> Result<(), &'static str> {
    use sharing_setup::{Connect, Phase};
    use std::sync::atomic::Ordering::SeqCst;
    let state = app.state::<AppState>();
    // The rclone found here, and whether it does: one that Windows or security software will not
    // run ends the setup, never replaced by a download of SlopBar's own (`rclone_step`).
    let found = tauri::async_runtime::spawn_blocking(|| rclone::find().map(|path| {
        let checked = rclone::check(&rclone::Cli::local(path.clone()));
        (path, checked)
    // A check that panicked found nothing out: it stops the setup, never leads to a download.
    })).await.map_err(|_| "setup_failed")?;
    let own = sharing_setup::own_rclone();
    let path = match sharing_setup::rclone_step(found, own.as_deref()) {
        sharing_setup::RcloneStep::Use(path) => path,
        sharing_setup::RcloneStep::Stop(code) => return Err(code),
        sharing_setup::RcloneStep::Download => {
            let own = own.ok_or("extract_failed")?;
            publish_sharing(app, |s| {
                s.downloaded = true;
                s.progress = Some(0);
            });
            let signal = app.clone();
            let progress = move |percent: u8| {
                publish_sharing(&signal, |s| s.progress = Some(percent));
            };
            let mut reasons = Vec::new();
            let result = sharing_setup::install_rclone(&own, &state.sharing_cancel, &progress, &mut reasons).await;
            publish_sharing(app, |s| {
                s.progress = None;
                // Why the download failed, shown with that failure only: a site that failed before
                // another served the zip is no failure, and unpacking or Cancel has its own.
                if matches!(result, Err("download_failed" | "hash_mismatch")) {
                    s.reasons = reasons;
                }
            });
            result?;
            own
        }
    };
    if state.sharing_cancel.load(SeqCst) {
        return Err("cancelled");
    }
    let (rclone_path, name) = (path.clone(), remote.clone());
    let how = tauri::async_runtime::spawn_blocking(move || -> Result<Option<Connect>, &'static str> {
        // A listing that failed is not an empty one: creating would overwrite a remote of that name.
        let listed = rclone::listed(&rclone::Cli::local(rclone_path)).ok_or("rclone_failed")?;
        match listed.iter().find(|r| r.name == name) {
            None => Ok(Some(Connect::Create)),
            Some(r) if r.kind != "dropbox" => Err("remote_taken"),
            Some(_) if reconnect || !reaches(&name) => Ok(Some(Connect::Reconnect)),
            Some(_) => Ok(None),
        }
    }).await.map_err(|_| "rclone_failed")??;
    if let Some(how) = how {
        publish_sharing(app, |s| s.phase = Phase::Dropbox);
        let (signal, rclone_path, name, cancel) = (app.clone(), path.clone(), remote.clone(), state.sharing_cancel.clone());
        tauri::async_runtime::spawn_blocking(move || {
            let on_link = |link: String| {
                // rclone opens no browser: SlopBar opens its page here, unless a browser on another
                // computer finishes the sign-in through a tunnel.
                if !via_tunnel {
                    let _ = offer_sharing_page(&signal, link.clone(), None);
                }
                publish_sharing(&signal, |s| {
                    s.waiting = true;
                    s.link = Some(link);
                });
            };
            sharing_setup::connect_dropbox(&rclone_path, &name, how, &on_link, &cancel, sharing_setup::SIGN_IN_TIMEOUT)
        }).await.map_err(|_| "rclone_failed")??;
        publish_sharing(app, |s| {
            s.waiting = false;
            s.link = None;
        });
        // rclone ends well only with the remote written; it must also reach Dropbox.
        let name = remote.clone();
        if !tauri::async_runtime::spawn_blocking(move || reaches(&name)).await.unwrap_or(false) {
            return Err("rclone_failed");
        }
    }
    publish_sharing(app, |s| s.phase = Phase::Sharing);
    {
        let held = state.settings_lock.lock().await;
        let mut next = state.settings.lock().unwrap().clone();
        next.presence_remote = Some(remote.clone());
        next.presence_folder = None;
        // Lets go of settings_lock; the refresh below withdraws what the move left behind.
        store_settings(app, held, next).map_err(|_| "settings_failed")?;
    }
    do_refresh(app).await;
    let settings = state.settings.lock().unwrap().clone();
    let found = tauri::async_runtime::spawn_blocking(move || {
        let Some(store) = presence::store(&settings) else { return vec![] };
        let own = presence::identity().map(|(id, _)| id.clone()).unwrap_or_default();
        let now = chrono::Utc::now();
        presence::peers(store.list().unwrap_or_default(), &own, now).into_iter()
            .filter(|p| p.live || now - p.seen_at <= chrono::Duration::days(1))
            .map(|p| p.machine)
            .collect::<Vec<_>>()
    }).await.unwrap_or_default();
    publish_sharing(app, |s| {
        s.phase = Phase::Done;
        s.found = found;
    });
    Ok(())
}

/// Whether the rclone remote `name` reaches its storage (lists SlopBar's folder there).
fn reaches(name: &str) -> bool {
    rclone::Rclone::open(name).is_some_and(|store| presence::PresenceStore::list(&store).is_ok())
}

/// Stops a running setup: rclone's sign-in ends and writes nothing.
fn cancel_sharing_setup_now(app: &AppHandle) -> sharing_setup::Setup {
    let state = app.state::<AppState>();
    state.sharing_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let setup = state.sharing_setup.lock().unwrap().clone();
    setup
}

/// Opens (or closes) the SSH tunnel for another computer's waiting sign-in, and this computer's
/// browser on the link built here from `state` alone.
async fn sharing_tunnel(app: &AppHandle, host: String, state: Option<String>, close: bool) -> Result<serde_json::Value, String> {
    let shared = app.state::<AppState>();
    if close {
        let mut tunnel = shared.sharing_tunnel.lock().unwrap();
        if tunnel.as_ref().is_some_and(|t| t.host == host) {
            *tunnel = None;
        }
        return Ok(serde_json::json!({ "closed": true }));
    }
    let link = state.as_deref().and_then(sharing_setup::link_for).ok_or("bad_state")?;
    desktop_shown()?;
    *shared.sharing_tunnel.lock().unwrap() = None;
    let target = host.clone();
    let tunnel = tauri::async_runtime::spawn_blocking(move || sharing_setup::Tunnel::open(&pairing::Ssh::exe(), &target))
        .await.map_err(|e| e.to_string())?.map_err(String::from)?;
    let opened = tunnel.opened;
    *shared.sharing_tunnel.lock().unwrap() = Some(tunnel);
    // Closed after the sign-in's own time limit at the latest.
    let watcher = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(sharing_setup::SIGN_IN_TIMEOUT + Duration::from_secs(120)).await;
        let state = watcher.state::<AppState>();
        let mut tunnel = state.sharing_tunnel.lock().unwrap();
        if tunnel.as_ref().is_some_and(|t| t.opened == opened) {
            *tunnel = None;
        }
    });
    let (opened, notified) = offer_sharing_page(app, link, Some(&host));
    Ok(serde_json::json!({ "opened": opened, "notified": notified }))
}

#[tauri::command]
fn setup_sharing(app: AppHandle, reconnect: Option<bool>) -> Result<sharing_setup::Setup, String> {
    start_sharing_setup(&app, None, false, reconnect.unwrap_or(false))
}

#[tauri::command]
fn cancel_sharing_setup(app: AppHandle) -> sharing_setup::Setup {
    cancel_sharing_setup_now(&app)
}

/// Opens Dropbox's page again (through rclone's own local link) when the user closed it.
#[tauri::command]
fn open_sharing_page(app: AppHandle) -> Result<(), String> {
    let link = app.state::<AppState>().sharing_setup.lock().unwrap().link.clone();
    let link = link.as_deref().and_then(sharing_setup::state_of).and_then(sharing_setup::link_for).ok_or("no_sign_in_waiting")?;
    open_in_browser(&link)
}

#[tauri::command]
fn sharing_setup_state(app: AppHandle) -> sharing_setup::Setup {
    app.state::<AppState>().sharing_setup.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------------------------
// Account roster (see `roster`)

/// Writes this machine's roster (with its "Sign in all accounts" run) when due and reads the
/// others' (at once with `force`, sooner with `soon`: see `roster::Syncer::sync`). Does nothing
/// without a sharing location or a passphrase, or once a save moved sharing elsewhere or turned
/// the roster off since `settings` were read (see `sharing::Sharing::sync_roster`).
async fn sync_roster(app: &AppHandle, settings: &settings::Settings, accounts: Vec<model::AccountUsage>, force: bool, soon: bool) {
    let state = app.state::<AppState>();
    let (Some(store), Some(passphrase)) = (roster::store(settings), settings::app_dir().and_then(|d| roster::load_passphrase(&d))) else {
        state.roster_seen.lock().unwrap().unsynced();
        return;
    };
    let (sharing, place, h) = (state.sharing.clone(), sharing::Place::of(settings), app.clone());
    let run = state.roster_run.lock().unwrap().clone();
    let now = chrono::Utc::now();
    let seen = tauri::async_runtime::spawn_blocking(move || {
        let (id, name) = presence::identity()?;
        let live = || sharing::Place::of(&h.state::<AppState>().settings.lock().unwrap());
        sharing.sync_roster(&place, live, |sync| {
            if let Some(run) = run {
                sync.offer_run(run);
            }
            // Read where the roster's lock is held, as the place is: a sync that started before
            // readings were turned off cannot write them after the write that took them out.
            sync.withhold_readings = !h.state::<AppState>().settings.lock().unwrap().share_readings;
            sync.sync(store.as_ref(), &passphrase, (id, name), &accounts, now, force, soon);
            sync.seen.clone()
        })
    }).await.ok().flatten();
    match seen {
        Some(seen) => *state.roster_seen.lock().unwrap() = seen,
        None => state.roster_seen.lock().unwrap().unsynced(),
    }
}

/// Sign-ins started through the door (often by an agent over SSH) open windows on this app's
/// desktop: refused while nobody can see it, so a sign-in never waits unseen (see `desktop`).
fn desktop_shown() -> Result<(), String> {
    match desktop::of_process().and_then(|d| d.problem()) {
        Some(problem) => Err(problem.into()),
        None => Ok(()),
    }
}

/// The door's `get_readiness`: whether sign-ins can run here, and what is waiting.
async fn readiness(app: &AppHandle) -> serde_json::Value {
    let roster = roster_status(app, false).await;
    let desktop = desktop::of_process();
    let browser = tauri::async_runtime::spawn_blocking(|| signin::find_browser().map(|(k, _)| pairing::browser_name(k))).await.ok().flatten();
    let (ready, mut problems, mut next) = pairing::assess(true, desktop, browser.is_some(), &roster);
    let state = app.state::<AppState>();
    // A remote named in settings shares nothing unless rclone here has it.
    let remote = state.settings.lock().unwrap().presence_remote.clone();
    if let Some(name) = remote {
        if !rclone_remotes().await.remotes.iter().any(|r| r.name == name) {
            problems.push("rclone_remote_missing");
            if ready {
                next = pairing::rclone_missing_next(&name);
            }
        }
    }
    // A registry read and a file check: off the async threads.
    let ssh_shell = tauri::async_runtime::spawn_blocking(pairing::ssh_shell).await.ok().flatten();
    pairing::note_ssh_shell(ssh_shell, &mut problems, &mut next);
    let snapshot = state.snapshot.lock().unwrap().clone();
    let signin_window = state.settings.lock().unwrap().signin_window;
    let mut summary = roster.clone();
    let waiting = summary.get_mut("waiting").map(serde_json::Value::take).unwrap_or_default();
    summary["waiting"] = waiting.as_array().map_or(0, Vec::len).into();
    serde_json::json!({
        "app": { "running": true, "version": app_version(app.clone()), "build": diagnostics::build_info(), "pid": std::process::id() },
        // The id its roster carries: the computer asking notes which SSH host reaches it.
        "machine": presence::identity().map(|(id, name)| serde_json::json!({ "id": id, "name": name })),
        "desktop": desktop, "idle_seconds": desktop::idle_seconds(), "browser": browser, "ssh_shell": ssh_shell, "signin_window": signin_window,
        "accounts": { "here": snapshot.accounts.len(), "unmonitored": bulk::unmonitored(&snapshot).len() },
        "roster": summary, "waiting": waiting,
        // Sign in all accounts as it runs: email g of emails, done of total, who is signing in
        // now and for what (add, renew or connect), why it stopped, and each step's outcome.
        "sign_in": state.bulk_connect.lock().unwrap().clone(),
        // Pass as `since` with `wait_seconds` to hear of the next change.
        "sign_in_revision": state.bulk_revision.load(std::sync::atomic::Ordering::SeqCst),
        "ready_for_sign_in": ready, "problems": problems, "next": next,
        // Another computer can complete this one's sign-ins in its own browser (`relay_start`):
        // the version of that protocol this app speaks, and whether one is open now.
        "relay": relay::VERSION, "relay_open": relay::SLOT.busy(),
    })
}

/// Settings > Other computers and the door's `get_roster`. With `sync`, writes and reads the
/// rosters first.
async fn roster_status(app: &AppHandle, sync: bool) -> serde_json::Value {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let snapshot = state.snapshot.lock().unwrap().clone();
    if sync && settings.account_roster {
        sync_roster(app, &settings, snapshot.accounts.clone(), true, false).await;
    }
    let seen = state.roster_seen.lock().unwrap().clone();
    let waiting = roster_waiting(&state, &snapshot);
    serde_json::json!({
        "enabled": settings.account_roster,
        "location": settings.presence_remote.is_some() || settings.presence_folder.is_some(),
        "passphrase_saved": roster_passphrase_saved(),
        "wrote": seen.wrote, "written_at": seen.written_at, "write_error": seen.write_error, "read": seen.read, "read_at": seen.read_at, "read_error": seen.read_error,
        "own_accounts": seen.own_accounts, "own_rewritten": seen.own_rewritten, "own_restores": seen.own_restores, "unreadable": seen.unreadable, "unsupported": seen.unsupported,
        "share_readings": settings.share_readings,
        "machines": seen.peers.iter().map(|p| serde_json::json!({ "machine": p.machine, "written_at": p.written_at, "accounts": p.accounts.len(), "readings": p.readings.len() })).collect::<Vec<_>>(),
        "waiting": waiting,
    })
}

#[tauri::command]
async fn get_roster(app: AppHandle, sync: Option<bool>) -> serde_json::Value {
    roster_status(&app, sync.unwrap_or(false)).await
}

/// Settings > Accounts' "Signing in on <machine>" cards: the other computers' runs of "Sign in all
/// accounts" as their rosters carry them (read sooner while Settings is `watching`; see
/// `roster::peer_runs`), and whether Retry can reach each one over SSH.
#[tauri::command]
async fn peer_sign_ins(app: AppHandle, watching: Option<bool>) -> Vec<serde_json::Value> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    if !settings.account_roster {
        return vec![];
    }
    let accounts = state.snapshot.lock().unwrap().accounts.clone();
    sync_roster(&app, &settings, accounts, false, watching.unwrap_or(false)).await;
    let dir = settings::app_dir();
    let runs = roster::peer_runs(&state.roster_seen.lock().unwrap(), chrono::Utc::now());
    runs.into_iter().map(|r| {
        let reachable = dir.as_deref().and_then(|d| pairing::host_for(d, &r.machine_id)).is_some();
        let mut card = serde_json::to_value(&r).unwrap_or_default();
        card["reachable"] = reachable.into();
        card
    }).collect()
}

/// What Retry on a "Signing in on <machine>" card did there (see `retry_peer_sign_in`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PeerRetry {
    /// Only the sign-in the card showed waiting starts again there, in the same window.
    Restarted,
    /// Sign in all accounts runs there again, asking only for what is still missing.
    Retried,
    /// Nothing: what the card showed has changed there (its sign-in moved on or started again, or
    /// another run goes), and stopping a run would undo what it did.
    MovedOn,
    /// Nothing: no sign-in is missing there any more, so Sign in all accounts started no run.
    NothingMissing,
}

/// What a card's Retry does next (see `after_peer_restart`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterRestart {
    /// Nothing more: the sign-in started again there, or it moved on.
    Done(PeerRetry),
    /// Sign in all accounts runs there again. Only with `stop` does a run still going there stop
    /// first: one left waiting where that computer cannot start one sign-in again, never a run that
    /// may have started there since the one the card showed ended.
    WholeRun { stop: bool },
}

/// Where a card's Retry goes after asking that computer to start the sign-in it showed waiting
/// again (`restart_sign_in`), from its answer: done; on to Sign in all accounts there when its run
/// has ended, or, stopping the run left waiting, when its SlopBar does not know the request;
/// nothing when the sign-in moved on; or the error, when the ask never reached its app (a whole
/// run would fail the same way).
fn after_peer_restart(answer: Result<serde_json::Value, String>) -> Result<AfterRestart, String> {
    match answer {
        Ok(a) if a["restarted"] == true => Ok(AfterRestart::Done(PeerRetry::Restarted)),
        Ok(a) if a["reason"] == "no_run" => Ok(AfterRestart::WholeRun { stop: false }),
        Ok(_) => Ok(AfterRestart::Done(PeerRetry::MovedOn)),
        Err(e) if crate::mcp::unknown_there(&e, "restart_sign_in") => Ok(AfterRestart::WholeRun { stop: true }),
        Err(e) => Err(e),
    }
}

/// `sign_in_all`'s arguments for a card's Retry: Sign in all accounts there again, with its run's
/// `monitoring`; only with `stop` does a run still going there stop first (`retry`).
fn whole_run_arguments(monitoring: bool, stop: bool) -> serde_json::Value {
    serde_json::json!({ "confirm": true, "monitoring": monitoring, "retry": stop })
}

/// What a card's Retry did once it asked that computer to run Sign in all accounts again (`stop`:
/// after stopping a run left waiting there), from its answer.
fn after_whole_run(ran: Result<serde_json::Value, String>, stop: bool) -> Result<PeerRetry, String> {
    match ran {
        Ok(answer) if answer["started"] == false => Ok(PeerRetry::NothingMissing),
        Ok(_) => Ok(PeerRetry::Retried),
        // A run goes there now, which this Retry never stops: it moved on.
        Err(e) if !stop && crate::mcp::code_there(&e, &["monitor_busy"]).is_some() => Ok(PeerRetry::MovedOn),
        Err(e) => Err(card_error(e)),
    }
}

/// An error from that computer as the card says it: a desktop nobody can see there by its code,
/// which Settings words (another computer prints `explain`'s words, meant for agents).
fn card_error(error: String) -> String {
    crate::mcp::code_there(&error, &["desktop_locked", "desktop_not_shown"]).map_or(error, str::to_string)
}

/// Where a card's Retry goes before asking that computer anything, from what the card shows: its
/// run still `going`, the start that names it (`started_at`, from a SlopBar that can start one
/// sign-in again) and its open sign-in's place (`step`).
fn peer_retry_first(going: bool, started_at: Option<chrono::DateTime<chrono::Utc>>, step: Option<usize>) -> Option<AfterRestart> {
    match (going, started_at, step) {
        // Ask it to start that sign-in again (`restart_sign_in`).
        (true, Some(_), Some(_)) => None,
        // A run left waiting where that computer cannot start one sign-in again: as before, it
        // stops and runs again.
        (true, None, _) => Some(AfterRestart::WholeRun { stop: true }),
        // Its run ended, or none of its sign-ins is open: Sign in all accounts runs again, never
        // stopping a run going there now (another run, or that one going on unseen).
        _ => Some(AfterRestart::WholeRun { stop: false }),
    }
}

/// Retry on a "Signing in on <machine>" card, through that computer's SSH door (see
/// `pairing::remote_call`), which must have been paired or checked from here (`pairing::host_for`).
/// While the run the card shows goes on (it started at `started_at`, and its sign-in `step` was the
/// open one), only that sign-in starts again there, in the same window, as the card says
/// (`restart_sign_in`); nothing when it moved on. Where that run has ended, Sign in all accounts
/// runs there again, never stopping a run going there; only where that computer's SlopBar is older
/// does a run left waiting there stop first, as before.
#[tauri::command]
async fn retry_peer_sign_in(app: AppHandle, machine_id: String, going: bool, started_at: Option<chrono::DateTime<chrono::Utc>>, step: Option<usize>) -> Result<PeerRetry, String> {
    let host = settings::app_dir().and_then(|d| pairing::host_for(&d, &machine_id)).ok_or("peer_not_paired")?;
    let next = match (peer_retry_first(going, started_at, step), started_at, step) {
        (Some(next), ..) => next,
        (None, Some(started_at), Some(at)) => {
            let (host, arguments) = (host.clone(), serde_json::json!({ "step": at, "run_started_at": started_at }));
            let answer = tauri::async_runtime::spawn_blocking(move || pairing::remote_call(&pairing::Ssh, &host, "restart_sign_in", &arguments))
                .await.map_err(|_| "ssh_failed".to_string())?;
            after_peer_restart(answer).map_err(card_error)?
        }
        (None, ..) => AfterRestart::WholeRun { stop: false },
    };
    let outcome = match next {
        AfterRestart::Done(outcome) => outcome,
        AfterRestart::WholeRun { stop } => {
            let monitoring = app.state::<AppState>().roster_seen.lock().unwrap().peers.iter()
                .find(|p| p.machine_id == machine_id).and_then(|p| p.run.as_ref()).is_none_or(|r| r.monitoring);
            let arguments = whole_run_arguments(monitoring, stop);
            let ran = tauri::async_runtime::spawn_blocking(move || pairing::remote_call(&pairing::Ssh, &host, "sign_in_all", &arguments))
                .await.map_err(|_| "ssh_failed".to_string())?;
            after_whole_run(ran, stop)?
        }
    };
    // What it did reaches the roster a few seconds later: read it at the live pace meanwhile. On the
    // blocking pool: a sync that hangs on a folder holds the roster's lock.
    let sharing = app.state::<AppState>().sharing.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || sharing.with_syncer(|sync| sync.expect_news(&machine_id, chrono::Utc::now()))).await;
    Ok(outcome)
}

#[tauri::command]
fn roster_passphrase_saved() -> bool {
    settings::app_dir().is_some_and(|d| roster::load_passphrase(&d).is_some())
}

/// Saves the roster passphrase with DPAPI (null deletes it), then writes and reads the rosters
/// with it. A new passphrase forgets the keys derived from the old one.
async fn set_roster_passphrase(app: &AppHandle, passphrase: Option<backup::Passphrase>) -> Result<serde_json::Value, String> {
    let dir = settings::app_dir().ok_or("roster_passphrase_storage")?;
    tauri::async_runtime::spawn_blocking(move || roster::store_passphrase(&dir, passphrase.as_ref()))
        .await.map_err(|_| "roster_passphrase_storage".to_string())??;
    let state = app.state::<AppState>();
    // On the blocking pool (a sync that hangs on a folder holds the roster's lock), and before the
    // sync below, which then starts from the new passphrase.
    let sharing = state.sharing.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || sharing.with_syncer(|sync| sync.reset())).await;
    *state.roster_seen.lock().unwrap() = roster::Seen::default();
    let status = roster_status(app, true).await;
    // What other machines' readings show and cover here follows the rosters this one opens.
    refresh_soon(app);
    Ok(status)
}

#[tauri::command]
async fn save_roster_passphrase(app: AppHandle, passphrase: Option<backup::Passphrase>) -> Result<serde_json::Value, String> {
    set_roster_passphrase(&app, passphrase).await
}

/// "Send test": the saved URL and token, whether or not phone alerts are on.
#[tauri::command]
async fn send_phone_test(app: AppHandle, title: String, body: String) -> Result<(), String> {
    let phone_settings = app.state::<AppState>().settings.lock().unwrap().phone.clone();
    let url = phone_settings.url.clone().ok_or("phone_url_missing")?;
    let message = phone::compose(&phone_settings, phone::Kind::Test, (&title, &body), (&title, &body)).ok_or("phone_url_missing")?;
    let token = settings::app_dir().and_then(|d| phone::load_token(&d));
    phone::send(&url, token.as_deref(), &message).await
}

/// Re-reads the Codex CLI's local history off the async runtime, one scan at a time, then has
/// open windows reload their report. Its first run reads weeks of rollouts and can take a while.
fn scan_codex_local(app: &AppHandle) {
    static SCANNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if SCANNING.swap(true, std::sync::atomic::Ordering::AcqRel) { return; }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let local = tauri::async_runtime::spawn_blocking(|| codex_local::scan(chrono::Utc::now())).await;
        SCANNING.store(false, std::sync::atomic::Ordering::Release);
        let Ok(local) = local else { return };
        let state = app.state::<AppState>();
        *state.codex_local.lock().unwrap() = Some(std::sync::Arc::new(local));
        let snapshot = state.snapshot.lock().unwrap().clone();
        let _ = app.emit("usage-updated", &snapshot);
        crate::alerts::on_snapshot(&app);
    });
}

/// Changes the automatic-connect state, saving it when `change` says it changed.
fn auto_monitor_saved(app: &AppHandle, change: impl FnOnce(&mut auto_monitor::Tracker) -> bool) {
    let state = app.state::<AppState>();
    let saved = {
        let mut tracker = state.auto_monitor.lock().unwrap();
        change(&mut tracker).then(|| tracker.saved.clone())
    };
    if let Some(saved) = saved {
        let _ = auto_monitor::save(&saved);
    }
}

/// Starts monitoring's sign-in for the account in use when it is due (see `auto_monitor`). Reads
/// the last snapshot, so it is cheap to call often.
fn auto_connect_tick(app: &AppHandle) {
    let state = app.state::<AppState>();
    // One sign-in at a time: never alongside an open one or a running "Connect monitoring for all".
    let queued = state.bulk_connect.lock().unwrap().as_ref().is_some_and(|b| !b.finished);
    if queued || state.account_operations.busy(monitoring::SIGN_IN) {
        return;
    }
    let enabled = state.settings.lock().unwrap().auto_connect_monitoring;
    let seen: Vec<_> = state.snapshot.lock().unwrap().accounts.iter().map(auto_monitor::Seen::of).collect();
    let fullscreen = enabled && dock::fullscreen();
    let busy = |key: &str| state.account_operations.busy(key);
    let guards = auto_monitor::Guards { enabled, fullscreen, busy: &busy };
    let decision = state.auto_monitor.lock().unwrap().decide(&seen, &guards, chrono::Utc::now());
    let auto_monitor::Decision::Start { provider, id } = decision else { return };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let key = format!("{}:{id}", provider.name());
        if let Err(e) = start_monitoring_login(app.clone(), provider, id, true, None, None).await {
            // Someone else started on it meanwhile: no reason to wait a day.
            let ok = e == "monitor_busy";
            auto_monitor_saved(&app, |t| t.finished(&key, ok, chrono::Utc::now()));
        }
    });
}

/// Refreshes on the interval from settings, re-reading it so changes apply without a restart.
async fn refresh_loop(app: AppHandle) {
    loop {
        do_refresh(&app).await;
        auto_connect_tick(&app);
        let started = Instant::now();
        let mut seconds = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            seconds += 1;
            // Every half minute, so an account is connected soon after its 3 minutes in use.
            if seconds % 30 == 0 { auto_connect_tick(&app); }
            let minutes = app.state::<AppState>().settings.lock().unwrap().refresh_minutes.max(1);
            if started.elapsed() >= Duration::from_secs(minutes as u64 * 60) {
                break;
            }
        }
    }
}

/// The app's context. An isolated run (`SLOPBAR_DATA_DIR`) takes an identifier of its own, so
/// its WebView2 data folder, single-instance mutex and notification id are its own too.
fn context() -> tauri::Context {
    let mut context = tauri::generate_context!();
    if settings::isolated() {
        let config = context.config_mut();
        config.identifier = format!("{}.isolated", config.identifier);
    }
    // Every window renders in software (see `additionalBrowserArgs` in tauri.conf.json);
    // SLOPBAR_GPU=1 brings the GPU process back, should a driver render something wrong.
    if std::env::var_os("SLOPBAR_GPU").is_some_and(|v| v == "1") {
        for w in context.config_mut().app.windows.iter_mut() {
            w.additional_browser_args = w.additional_browser_args.take().map(|a| a.replace(" --disable-gpu", ""));
        }
    }
    context
}

/// Whether an exit request may end the app. Tauri asks with no code when its last window closes,
/// which here only means the flyout was dismissed; an explicit exit (Quit, restart, update) carries one.
fn exit_allowed(code: Option<i32>) -> bool {
    code.is_some()
}

#[cfg(test)]
mod sign_in_tests {
    use super::*;

    #[test]
    fn a_codex_sign_in_with_the_right_email_is_a_workspace_mismatch() {
        let (a, b) = (Some("synthetic.one@example.invalid"), Some("Synthetic.One@example.invalid "));
        assert_eq!(wrong_account(Provider::Codex, a, b), "monitor_wrong_workspace");
        assert_eq!(wrong_account(Provider::Codex, a, Some("synthetic.two@example.invalid")), "monitor_wrong_account");
        assert_eq!(wrong_account(Provider::Codex, None, b), "monitor_wrong_account");
        assert_eq!(wrong_account(Provider::Claude, a, b), "monitor_wrong_account", "a Claude organization is not a workspace choice");
    }

    #[test]
    fn a_run_reports_every_step() {
        let step = StepRecord { target: bulk::Target { provider: Provider::Codex, id: "synthetic-id".into(), account: Some("synthetic@example.invalid".into()), ended: None },
            kind: bulk::Kind::Connect, machine: None, state: StepState::Failed, error: Some("signin_no_page".into()), started_at: None, page_at: None, ended_at: None };
        let v = serde_json::to_value(&step).unwrap();
        assert_eq!(v["provider"], "codex");
        assert_eq!(v["id"], "synthetic-id");
        assert_eq!(v["kind"], "connect");
        assert_eq!(v["state"], "failed");
        assert_eq!(v["error"], "signin_no_page");
        let done = StepRecord { state: StepState::Done, error: None, ..step.clone() };
        assert!(serde_json::to_value(&done).unwrap().get("error").is_none());
        // The same run as the roster carries it to the other computers.
        let started: chrono::DateTime<chrono::Utc> = "2026-10-23T11:58:00.123Z".parse().unwrap();
        let bulk = BulkConnect { run_started_at: started, total: 2, done: 1, current: None, kind: None, emails: 1, email: 1, finished: true, error: Some("signin_no_page".into()),
            steps: vec![done, step], by_email: true, monitoring: true, still_waiting: false, revision: 9, everywhere: false, left_out: vec![], in_use: vec![] };
        let run = roster_run(&bulk, "2026-10-23T12:00:00Z".parse().unwrap());
        assert_eq!((run.revision, run.finished, run.error.as_deref()), (9, true, Some("signin_no_page")));
        // The roster names the run as readiness does, so a card's Retry can name it back; readiness
        // keeps it apart from each step's own start.
        assert_eq!(run.started_at, Some(started));
        let shown = serde_json::to_value(&bulk).unwrap();
        assert_eq!((&shown["run_started_at"], shown.get("started_at")), (&serde_json::json!("2026-10-23T11:58:00.123Z"), None));
        assert_eq!(run.open, None, "nothing is open once it ended");
        assert_eq!(roster_run(&open_run(true), "2026-11-23T10:05:00Z".parse().unwrap()).open, Some(1), "the open sign-in's place, as this computer counts it");
        assert_eq!(run.steps.iter().map(|s| (s.kind.as_str(), s.state.as_str())).collect::<Vec<_>>(), [("connect", "done"), ("connect", "failed")]);
        assert_eq!(run.steps[1].account.as_deref(), Some("synthetic@example.invalid"));
    }

    #[test]
    fn retry_this_sign_in_starts_only_the_one_asked_for_and_once() {
        let restart = StepRestart::default();
        assert!(!restart.take(3), "nothing asked");
        restart.ask(3);
        assert!(!restart.take(4), "asked for another of the run's sign-ins");
        assert!(restart.take(3) && !restart.take(3), "taken once");
        restart.ask(3);
        assert!(!restart.take(3), "a second ask right after (a double click) is the same one");
        restart.ask(5);
        restart.begin();
        assert!(!restart.take(5), "an ask from before a sign-in's turn is not for it");
        restart.ask(5);
        assert!(restart.take(5), "a new turn takes a new ask at once");
    }

    /// "Retry this sign-in" never drops the email's window (its Google sign-in), drops the old
    /// sign-in before waiting for it to let go of this computer's one sign-in, and only then starts
    /// the new one; the watches count from the new start.
    #[test]
    fn retry_this_sign_in_keeps_the_window_and_starts_anew() {
        let code = crate::source_guard::tokens(include_str!("app.rs"));
        let run = crate::source_guard::function(&code, "app.rs", "fn run_sign_ins(", "app::sign_in_tests");
        let at = crate::source_guard::find(run, &crate::source_guard::tokens("_ = restart.notify.notified() => {")).expect("the local sign-in's restart arm");
        let arm = &run[at..];
        let arm = &arm[..crate::source_guard::find(arm, &crate::source_guard::tokens("s.rearm();")).expect("the arm's end")];
        for needle in ["if !restart.take(at) { continue; }", "drop(std::mem::replace(&mut run, Box::pin(start())));",
            "let held = [monitoring::SIGN_IN.to_string(), format!(\"{}:{}\", target.provider.name(), target.id),",
            "while held.iter().any(|key| h.state::<AppState>().account_operations.busy(key)) {",
            "watch = Box::pin(watch_step(&h, session.as_deref(), since, false, &turn));", "opened = Box::pin(page_opened(session.as_deref(), since));"] {
            assert!(crate::source_guard::has(arm, &crate::source_guard::tokens(needle)), "{needle}\n{arm}");
        }
        let (dropped, waited) = (crate::source_guard::find(arm, "drop(std::mem::replace").unwrap(), crate::source_guard::find(arm, "while held").unwrap());
        assert!(dropped < waited, "the old sign-in goes before the wait for it");
        assert!(!arm.contains("group_session") && !arm.contains("drop(session"), "the email's window stays");
    }

    /// A run of three sign-ins whose second is open, as `restartable` judges it.
    fn open_run(still_waiting: bool) -> BulkConnect {
        let step = |state| StepRecord { target: bulk::Target { provider: Provider::Claude, id: "synthetic-id".into(), account: Some("synthetic@example.invalid".into()), ended: None },
            kind: bulk::Kind::Connect, machine: None, state, error: None, started_at: None, page_at: None, ended_at: None };
        BulkConnect { run_started_at: "2026-11-23T10:00:00.5Z".parse().unwrap(), total: 3, done: 1, current: None, kind: None, emails: 1, email: 1, finished: false, error: None,
            steps: vec![step(StepState::Done), step(StepState::Running), step(StepState::Pending)], by_email: true, monitoring: true, still_waiting,
            revision: 4, everywhere: false, left_out: vec![], in_use: vec![] }
    }

    /// From elsewhere, only the open sign-in of the run named, once it has waited: never another
    /// run's, one that ended or has not started, or one somebody may still be completing.
    #[test]
    fn a_sign_in_starts_again_from_elsewhere_only_when_it_is_the_one_waiting() {
        let run = open_run(true);
        let started = run.run_started_at;
        assert_eq!(restartable(Some(&run), 1, started), Ok(()));
        assert_eq!(restartable(None, 1, started), Err(NotRestarted::NoRun), "nothing ran since the app started");
        assert_eq!(restartable(Some(&BulkConnect { finished: true, ..open_run(true) }), 1, started), Err(NotRestarted::NoRun), "it ended");
        let later = started + chrono::Duration::nanoseconds(1);
        assert_eq!(restartable(Some(&run), 1, later), Err(NotRestarted::OtherRun), "a newer run is not the one the card showed");
        for at in [0, 2, 3] {
            assert_eq!(restartable(Some(&run), at, started), Err(NotRestarted::NotOpen), "done, pending or past the end: {at}");
        }
        let mut between = open_run(true);
        between.steps[1].state = StepState::Done;
        assert_eq!(restartable(Some(&between), 1, started), Err(NotRestarted::NotOpen), "between two sign-ins nothing is open");
        assert_eq!(restartable(Some(&open_run(false)), 1, started), Err(NotRestarted::NotWaiting), "someone may still be completing it");
        // A sitting for every computer: its open sign-in may be a third computer's, which a request
        // from elsewhere never reaches on to; the person at its screen has Retry this sign-in.
        assert_eq!(restartable(Some(&BulkConnect { everywhere: true, ..open_run(true) }), 1, started), Err(NotRestarted::Everywhere));
        // Checked in this order: a stale card hears what changed first.
        assert_eq!(restartable(Some(&open_run(false)), 2, later), Err(NotRestarted::OtherRun));
        assert_eq!(restartable(Some(&BulkConnect { everywhere: true, finished: true, ..open_run(true) }), 1, later), Err(NotRestarted::NoRun));
    }

    /// Whatever `restart_sign_in` answers reaches the agent whole (mcp::MAX_TEXT), and says what
    /// happened in the words of its reason.
    #[test]
    fn what_restart_sign_in_tells_an_agent_reaches_it_whole() {
        let reasons = [NotRestarted::NoRun, NotRestarted::Everywhere, NotRestarted::OtherRun, NotRestarted::NotOpen, NotRestarted::NotWaiting];
        for text in std::iter::once(RESTARTED_NEXT).chain(reasons.map(not_restarted_next)) {
            let n = text.chars().count();
            assert!(n <= crate::mcp::MAX_TEXT, "{n} characters: {text}");
            assert!(!text.contains("  ") && !text.contains("SlopBar"), "one space between words, no brand: {text}");
        }
        assert_eq!(reasons.map(|r| serde_json::to_value(r).unwrap()), ["no_run", "everywhere", "other_run", "not_open", "not_waiting"].map(serde_json::Value::from));
    }

    /// The card's Retry: only the sign-in it showed starts again where that computer can do so;
    /// the whole run runs again where its run ended or its SlopBar is older; nothing when the
    /// sign-in moved on (a whole run would stop a run that moved on, or one someone else started).
    #[test]
    fn a_card_s_retry_runs_everything_again_only_where_the_run_ended_or_the_computer_is_older() {
        let said = |v: serde_json::Value| after_peer_restart(Ok(v));
        assert_eq!(said(serde_json::json!({ "restarted": true, "step": 1 })), Ok(AfterRestart::Done(PeerRetry::Restarted)));
        // Its run ended: Sign in all accounts again, without stopping a run started there since.
        assert_eq!(said(serde_json::json!({ "restarted": false, "reason": "no_run" })), Ok(AfterRestart::WholeRun { stop: false }));
        for reason in ["everywhere", "other_run", "not_open", "not_waiting", "a_reason_from_a_newer_version"] {
            assert_eq!(said(serde_json::json!({ "restarted": false, "reason": reason })), Ok(AfterRestart::Done(PeerRetry::MovedOn)), "{reason}");
        }
        assert_eq!(said(serde_json::json!({})), Ok(AfterRestart::Done(PeerRetry::MovedOn)), "an answer it cannot read stops nothing");
        // An older slopbar-mcp there (`step`'s own words), or an older app behind a newer helper:
        // as before, the run left waiting there stops and runs again.
        assert_eq!(after_peer_restart(Err("Unknown tool: restart_sign_in".into())), Ok(AfterRestart::WholeRun { stop: true }));
        assert_eq!(after_peer_restart(Err(crate::mcp::explained("unsupported_request"))), Ok(AfterRestart::WholeRun { stop: true }));
        for error in ["ssh_failed: Connection timed out", "SlopBar is not running", "Nothing was started: this computer's desktop is locked", "Unknown tool: sign_in_all"] {
            assert_eq!(after_peer_restart(Err(error.into())), Err(error.into()), "a whole run would fail the same way");
        }
    }

    /// Sign in all accounts asked for again there: a run started, nothing was missing, a run that goes
    /// there now (never stopped unless the card meant to), or an error as the card words it.
    #[test]
    fn a_card_s_whole_run_says_what_it_did_there() {
        assert_eq!(after_whole_run(Ok(serde_json::json!({ "started": true, "plan": [] })), false), Ok(PeerRetry::Retried));
        assert_eq!(after_whole_run(Ok(serde_json::json!({ "started": false, "plan": [] })), true), Ok(PeerRetry::NothingMissing));
        let busy = crate::mcp::explained("monitor_busy");
        assert_eq!(after_whole_run(Err(busy.clone()), false), Ok(PeerRetry::MovedOn), "a run going there is someone's: left alone");
        assert_eq!(after_whole_run(Err(busy.clone()), true), Err(busy));
        assert_eq!(after_whole_run(Err(crate::mcp::explained("desktop_locked")), false), Err("desktop_locked".into()), "worded by Settings");
        assert_eq!(after_whole_run(Err("ssh_timeout".into()), true), Err("ssh_timeout".into()));
        assert_eq!(serde_json::to_value(PeerRetry::NothingMissing).unwrap(), "nothing_missing");
        // Only a card that meant to stop the run left waiting asks for that.
        assert_eq!(whole_run_arguments(true, false), serde_json::json!({ "confirm": true, "monitoring": true, "retry": false }));
        assert_eq!(whole_run_arguments(false, true)["retry"], true);
        // The card's whole run takes its `stop` from where it was decided, for its arguments and its answer.
        let code = crate::source_guard::tokens(include_str!("app.rs"));
        let retry = crate::source_guard::function(&code, "app.rs", "async fn retry_peer_sign_in(", "app::sign_in_tests");
        let arm = &retry[crate::source_guard::find(retry, "AfterRestart::WholeRun { stop } => {").expect("the whole run's arm")..];
        for needle in ["let arguments = whole_run_arguments(monitoring, stop);", "after_whole_run(ran, stop)?"] {
            assert!(crate::source_guard::has(arm, needle), "{needle}
{arm}");
        }
        assert!(!crate::source_guard::has(retry, "\"retry\": true"), "no Retry stops a run but by `stop`");
    }

    /// Before asking anything: a run going on that names its start and its open sign-in asks for
    /// just that one; a run left waiting where that computer cannot do that stops and runs again,
    /// as before; anything else runs again without stopping what goes there now.
    #[test]
    fn a_card_s_retry_stops_a_run_only_where_that_computer_cannot_restart_one_sign_in() {
        let at = Some("2026-11-23T10:00:00Z".parse().unwrap());
        assert_eq!(peer_retry_first(true, at, Some(2)), None);
        assert_eq!(peer_retry_first(true, None, Some(2)), Some(AfterRestart::WholeRun { stop: true }));
        assert_eq!(peer_retry_first(true, None, None), Some(AfterRestart::WholeRun { stop: true }));
        assert_eq!(peer_retry_first(true, at, None), Some(AfterRestart::WholeRun { stop: false }), "none of its sign-ins is open: never stop it");
        for (started, step) in [(at, Some(2)), (None, None), (at, None), (None, Some(1))] {
            assert_eq!(peer_retry_first(false, started, step), Some(AfterRestart::WholeRun { stop: false }), "its run ended");
        }
    }

    /// A sign-in whose window was closed waits on nobody while it may still be saving, so a Retry
    /// from elsewhere hears it is not waiting rather than that it started again; nor does one that
    /// ended, before the next starts.
    #[test]
    fn a_closed_window_s_sign_in_is_not_waiting() {
        let code = crate::source_guard::tokens(include_str!("app.rs"));
        let run = crate::source_guard::function(&code, "app.rs", "fn run_sign_ins(", "app::sign_in_tests");
        let mark = crate::source_guard::statement(run, "let mark = |at: usize, state: StepState, error: Option<String>|").expect("the run's mark");
        assert!(crate::source_guard::has(mark, "if state != StepState::Running { b.still_waiting = false; }"), "{mark}");
        for (signature, publish) in [("fn run_sign_ins(", "publish_bulk(&h, |b| b.still_waiting = false);"), ("async fn relayed_turn(", "publish_bulk(h, |b| b.still_waiting = false);")] {
            let body = crate::source_guard::function(&code, "app.rs", signature, "app::sign_in_tests");
            let arm = &body[crate::source_guard::find(body, "_ = &mut closed => {").unwrap_or_else(|| panic!("{signature}: the closed window's arm"))..];
            let (published, grace) = (crate::source_guard::find(arm, publish), crate::source_guard::find(arm, "tokio::time::timeout(SIGN_IN_GRACE, &mut run)"));
            assert!(published.zip(grace).is_some_and(|(p, g)| p < g), "{signature}\n{arm}");
        }
    }

    #[test]
    fn restart_sign_in_names_its_sign_in_and_its_run() {
        let asked: crate::door::Request = serde_json::from_value(serde_json::json!({ "op": "restart_sign_in", "step": 2, "run_started_at": "2026-11-23T10:00:00.5Z" })).unwrap();
        let at: chrono::DateTime<chrono::Utc> = "2026-11-23T10:00:00.5Z".parse().unwrap();
        assert!(matches!(asked, crate::door::Request::RestartSignIn { step: 2, run_started_at } if run_started_at == at));
        for refused in [serde_json::json!({ "op": "restart_sign_in", "step": 2 }), serde_json::json!({ "op": "restart_sign_in", "run_started_at": "2026-11-23T10:00:00Z" }),
            serde_json::json!({ "op": "restart_sign_in", "step": -1, "run_started_at": "2026-11-23T10:00:00Z" }), serde_json::json!({ "op": "restart_sign_in", "step": 1, "run_started_at": "soon" }),
            serde_json::json!({ "op": "restart_sign_in", "at": 1, "started_at": "2026-11-23T10:00:00Z" }),
            serde_json::json!({ "op": "restart_sign_in", "step": 1, "run_started_at": "2026-11-23T10:00:00Z", "everywhere": true })] {
            assert!(serde_json::from_value::<crate::door::Request>(refused.clone()).is_err(), "{refused}");
        }
    }

    /// The door's restart judges the run and takes its restart under one hold of `bulk_connect` (a
    /// new run sets both under it), and asks only once `restartable` passed.
    #[test]
    fn a_restart_from_elsewhere_asks_the_run_it_judged() {
        let code = crate::source_guard::tokens(include_str!("app.rs"));
        let body = crate::source_guard::function(&code, "app.rs", "fn restart_from_elsewhere(", "app::sign_in_tests");
        let at = |needle: &str| crate::source_guard::find(body, &crate::source_guard::tokens(needle)).unwrap_or_else(|| panic!("{needle}\n{body}"));
        let (held, judged, taken, asked) = (at("let running = state.bulk_connect.lock().unwrap();"), at("restartable(running.as_ref(), step, run_started_at)?;"),
            at("let restart = state.bulk_restart.lock().unwrap().clone()"), at("restart.ask(step);"));
        assert!(held < judged && judged < taken && taken < asked, "{body}");
        assert!(!body.contains("drop(running)") && !body[held..asked].contains('}'), "the hold lasts until the ask: no block ends it");
        let door = crate::source_guard::function(&code, "app.rs", "async fn door_call(", "app::sign_in_tests");
        let arm = &door[crate::source_guard::find(door, &crate::source_guard::tokens("R::RestartSignIn { step, run_started_at } => {")).expect("the door's arm")..];
        let (shown, restarted) = (crate::source_guard::find(arm, "desktop_shown()?;").unwrap(), crate::source_guard::find(arm, "restart_from_elsewhere(").unwrap());
        assert!(shown < restarted, "refused while nobody can see this desktop");
    }

    #[test]
    fn retry_from_another_computer_is_opt_in() {
        let plain: crate::door::Request = serde_json::from_value(serde_json::json!({ "op": "sign_in_all", "confirm": true })).unwrap();
        assert!(matches!(plain, crate::door::Request::SignInAll { confirm: true, monitoring: true, retry: false, everywhere: false }));
        let retry: crate::door::Request = serde_json::from_value(serde_json::json!({ "op": "sign_in_all", "confirm": true, "retry": true })).unwrap();
        assert!(matches!(retry, crate::door::Request::SignInAll { retry: true, .. }));
    }

    #[test]
    fn readiness_waits_only_when_asked_and_old_apps_still_read_it() {
        let plain = serde_json::to_value(crate::door::Request::GetReadiness { wait_seconds: None, since: None }).unwrap();
        assert_eq!(plain, serde_json::json!({ "op": "get_readiness" }), "no new fields for an app that does not know them");
        let waiting: crate::door::Request = serde_json::from_value(serde_json::json!({ "op": "get_readiness", "wait_seconds": 30, "since": 7 })).unwrap();
        assert!(matches!(waiting, crate::door::Request::GetReadiness { wait_seconds: Some(30), since: Some(7) }));
    }
}

#[cfg(test)]
mod sign_in_open_tests;

#[cfg(test)]
mod everywhere_text_tests {
    #[test]
    fn what_sign_in_all_everywhere_tells_an_agent_reaches_it_whole() {
        // Every string an agent gets is cut at mcp::MAX_TEXT.
        for text in [super::EVERYWHERE_PREVIEW, super::EVERYWHERE_ASKED, super::EVERYWHERE_UNASKED, super::EVERYWHERE_NOTHING] {
            let n = text.chars().count();
            assert!(n <= crate::mcp::MAX_TEXT, "{n} characters: {text}");
            assert!(!text.contains("  "), "one space between words: {text}");
        }
    }

    #[test]
    fn an_alias_for_no_account_says_what_to_do_before_the_key() {
        // However long the key an agent sent, the advice before it reaches the agent (mcp::MAX_TEXT).
        let said = super::no_alias_account(&"k".repeat(2000));
        assert!(said.starts_with("alias keys are \"provider:id\" from get_usage; no account has the key \"kkk"), "{}", &said[..80]);
    }
}

#[cfg(test)]
mod exit_tests {
    #[test]
    fn closing_the_last_window_never_quits_but_an_explicit_exit_does() {
        assert!(!super::exit_allowed(None), "dismissing the flyout must not quit SlopBar");
        assert!(super::exit_allowed(Some(0)));
        assert!(super::exit_allowed(Some(1)));
    }
}

pub fn run() {
    // Before any HTTPS client, SlopBar's or the updater's: reqwest cannot build one without it.
    crate::net::install_crypto();
    migrate::run();
    let diagnostics = std::sync::Arc::new(diagnostics::Recorder::new(settings::app_dir().expect("config directory").join("diagnostics/history.bin")));
    let loaded = settings::load();
    // Off unless the user turned crash records on; nothing is recorded before the journal starts.
    diagnostics::set_crash_records(loaded.crash_records);
    diagnostics::install_crash_hook(diagnostics.clone());
    // Managed before the builder runs: config windows load (and call commands) before `setup`.
    let state = AppState {
        snapshot: Mutex::default(),
        settings: Mutex::new(loaded),
        menu: Mutex::default(),
        menu_items: Mutex::default(),
        panel_anchor: Mutex::default(),
        panel_height: Mutex::new(320.0),
        panel_hidden_at: Mutex::default(),
        dock_width: Mutex::new(160.0),
        usage_cache: Mutex::new(providers::load_cache()),
        cache_due: Default::default(),
        usage_history: Mutex::new(subscriptions::load()),
        codex_local: Mutex::default(),
        active_log: Mutex::new(active_log::load()),
        active_saved: Mutex::default(),
        usage_index: Mutex::default(),
        monitor: monitoring::Monitor::new(settings::app_dir().expect("config directory").join("monitoring")).with_diagnostics(diagnostics.clone()),
        diagnostics: diagnostics.clone(),
        refresh_lock: tokio::sync::Mutex::new(()),
        settings_lock: tokio::sync::Mutex::new(()),
        account_operations: monitoring::Operations::default(),
        sign_ins: monitoring::SignIns::default(),
        last_switch: Mutex::default(),
        wind_down: Mutex::default(),
        recent: Mutex::default(),
        sources: Mutex::default(),
        presence_tracks: Mutex::default(),
        presence_recall: Mutex::default(),
        // What saves left for the next refresh to withdraw, kept between runs (see `sharing`).
        sharing: std::sync::Arc::new(sharing::Sharing::kept_in(settings::app_dir().expect("config directory").join(sharing::DUE_FILE))),
        roster_seen: Mutex::default(),
        roster_run: Mutex::default(),
        unexpected_log: Mutex::new(unexpected::load()),
        plan_alerts: Mutex::new(plan_alerts::load()),
        burn_log: Mutex::new(burn::load()),
        burn_due: Mutex::default(),
        ledger: Mutex::new(ledger::load()),
        auto_monitor: Mutex::new(auto_monitor::Tracker::new(auto_monitor::load())),
        health: Mutex::default(),
        profile_states: Mutex::default(),
        bulk_connect: Mutex::default(),
        bulk_stop: Mutex::default(),
        bulk_restart: Mutex::default(),
        bulk_changed: tokio::sync::Notify::new(),
        bulk_revision: Default::default(),
        hotkey_error: Mutex::default(),
        compact_hotkey_error: Mutex::default(),
        compact_item: Mutex::default(),
        settings_page: Mutex::default(),
        leases: Mutex::default(),
        update_check: Default::default(),
        sharing_setup: Mutex::default(),
        sharing_cancel: Arc::default(),
        sharing_tunnel: Mutex::default(),
        everywhere: Mutex::default(),
        everywhere_ask: Mutex::default(),
    };
    tauri::Builder::default()
        // Must come first: a second launch hands over to the running instance and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_settings(app)))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new()
            .with_handler(|app, shortcut, event| {
                if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                    hotkey_pressed(app, shortcut);
                }
            })
            .build())
        .manage(state)
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            plan_sign_in_everywhere,
            start_sign_in_everywhere,
            take_everywhere_ask,
            get_usage,
            get_health,
            restart_app,
            get_subscriptions,
            get_sparklines,
            rescan_wsl,
            hide_panel,
            get_usage_breakdown,
            export_csv,
            csv_default_folder,
            export_backup,
            restore_backup,
            phone_token_saved,
            get_roster,
            peer_sign_ins,
            retry_peer_sign_in,
            roster_passphrase_saved,
            save_roster_passphrase,
            set_phone_token,
            send_phone_test,
            take_plan_alerts,
            take_capacity_alerts,
            get_credit_advice,
            take_digest,
            export_calendar,
            check_calendar_path,
            default_calendar_path,
            open_billing_page,
            export_diagnostics,
            record_frontend_error,
            refresh,
            get_settings,
            sync_folders,
            check_presence_folder,
            rclone_remotes,
            setup_sharing,
            cancel_sharing_setup,
            sharing_setup_state,
            open_sharing_page,
            system_locale,
            this_machine,
            check_for_updates,
            install_update,
            app_version,
            updates_enabled,
            save_settings,
            open_settings,
            take_settings_page,
            set_panel_height,
            set_menu_labels,
            switch_account,
            add_account,
            connect_monitoring,
            disconnect_monitoring,
            revoke_account_sessions,
            remove_account,
            connect_monitoring_all,
            sign_in_all_accounts,
            forget_signin_profiles,
            stop_sign_ins,
            retry_sign_ins,
            restart_sign_in,
            remove_lapsed_accounts,
            bulk_status,
            list_project_pins,
            set_project_pin,
            open_switcher,
            hide_switcher,
            hotkey_status,
            toggle_compact,
            set_dock_width,
            dock_clicked,
            dock_menu,
            accent_colors,
            notify::notify,
            notify::notify_unexpected,
            has_mica,
            taskbar_theme
        ])
        .setup(|app| {
            app.state::<AppState>().diagnostics.start();
            // Old diagnostics exports and window captures would otherwise pile up for ever.
            tauri::async_runtime::spawn_blocking(|| {
                if let Some(dir) = door::exports_dir() { diagnostics::prune_exports(&dir, std::time::SystemTime::now()); }
                if let Some(dir) = inspect::captures_dir() { inspect::prune(&dir); }
                // The browser profiles earlier versions kept per email (they offered stale Google
                // accounts), and fresh ones a SlopBar that ended mid-sign-in left behind.
                let _ = signin::cleanup();
            });
            // Before the first refresh reads the vault; a slot that fails stays readable as plaintext.
            let vault = accounts::migrate_vault();
            if !vault.is_empty() {
                let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
                app.state::<AppState>().diagnostics.record(None, None, diagnostics::Event::Vault {
                    migrated: count(vault.migrated), failed: count(vault.failed), unreadable: count(vault.unreadable),
                });
            }
            let handle = app.handle();
            #[cfg(windows)]
            {
                let registered = notify::register_app_id(&app.config().identifier);
                app.state::<AppState>().health.lock().unwrap().notifications_registered = Some(registered);
            }

            let defaults = ["Refresh", "Settings…", "Quit"];
            let items = MENU_IDS
                .iter()
                .zip(defaults)
                .map(|(id, text)| MenuItem::with_id(app, *id, text, true, None::<&str>))
                .collect::<Result<Vec<_>, _>>()?;
            let compact_on = app.state::<AppState>().settings.lock().unwrap().compact_flyout;
            let compact = CheckMenuItem::with_id(app, MENU_COMPACT, "Compact flyout", true, compact_on, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&items[0], &compact, &items[1], &separator, &items[2]])?;
            *app.state::<AppState>().menu.lock().unwrap() = Some(menu);
            *app.state::<AppState>().menu_items.lock().unwrap() = items;
            *app.state::<AppState>().compact_item.lock().unwrap() = Some(compact);
            app.on_menu_event(|app, event| menu_event(app, event.id().as_ref()));

            apply_settings(handle);
            apply_hotkey(handle);
            dock::show_docked(handle);
            // Built hidden now, so the first click on the widget opens the flyout at once.
            panel::prepare(handle);

            // Updates install silently; after one, say so once in the widget.
            let current = app.package_info().version.to_string();
            let previous = {
                let state = app.state::<AppState>();
                let mut s = state.settings.lock().unwrap();
                let previous = s.last_version.replace(current.clone());
                let _ = settings::save(&s);
                previous
            };
            if previous.is_some_and(|p| p != current) {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    let _ = h.emit("update-installed", current);
                });
            }

            let h = handle.clone();
            std::thread::spawn(move || dock::keep_docked(h));
            tauri::async_runtime::spawn(refresh_loop(handle.clone()));
            if !settings::isolated() {
                tauri::async_runtime::spawn(updates::run_loop(handle.clone()));
            }
            // Agents use Settings' operations through `slopbar-mcp`, which knocks here.
            #[cfg(windows)]
            {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    door::serve(door::pipe_name(), move |request| door_reply(h.clone(), request)).await;
                });
            }
            Ok(())
        })
        .build(context())
        .expect("error while building slopbar")
        .run(|app, event| {
            // The taskbar widget is native and every web window is built on demand, so closing
            // the flyout can close the last Tauri window. Tauri then asks to exit (code None);
            // SlopBar lives in the taskbar, so only an explicit exit (Quit, restart, update) ends it.
            if let tauri::RunEvent::ExitRequested { code, api, .. } = &event {
                if !exit_allowed(*code) {
                    api.prevent_exit();
                }
                return;
            }
            // App::run exits the process directly: code after it cannot flush a journal.
            if matches!(event, tauri::RunEvent::Exit) {
                on_exit(app);
            }
        });
}

/// What SlopBar does as it exits. Each part that can wait has a limit, so exiting never hangs on a
/// stuck disk, network folder or lock.
pub(crate) fn on_exit(app: &AppHandle) {
    use crate::update_check::{wait_at_most, EXIT_PART_LIMIT};
    // Other machines stop showing this one's accounts as in use, where sharing is now and where
    // saves moved it from: what they left that no refresh has withdrawn yet (a refresh still
    // running, or waiting for the refresh lock), which the next start withdraws if this runs out
    // of time (see `sharing::Sharing::exit`).
    let h = app.clone();
    wait_at_most(EXIT_PART_LIMIT, move || {
        let state = h.state::<AppState>();
        let settings = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let files = sharing::Files { interval: settings.refresh_minutes.max(1) as u64 * 60 };
        state.sharing.exit(&sharing::Place::of(&settings), &files, sharing::WITHDRAW_LIMIT);
    });
    // A sign-in still open loses its browser and fresh profile with SlopBar (the next start deletes
    // what it had no time for).
    wait_at_most(EXIT_PART_LIMIT, signin::close_all);
    // The journal waits for its writer a few seconds at most (`Recorder::flush`).
    app.state::<AppState>().diagnostics.record(None, None, diagnostics::Event::Shutdown);
}

#[cfg(test)]
mod window_tests {
    use super::*;

    fn windows() -> Vec<serde_json::Value> {
        let config: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        config["app"]["windows"].as_array().unwrap().clone()
    }

    #[test]
    fn settings_and_the_switcher_are_created_only_when_opened() {
        for w in windows() {
            let label = w["label"].as_str().unwrap();
            // `create` defaults to true.
            let at_startup = w["create"].as_bool().unwrap_or(true);
            assert_eq!(at_startup, !ON_DEMAND.contains(&label), "{label}");
        }
        for label in ON_DEMAND {
            assert!(windows().iter().any(|w| w["label"] == label), "{label} has a configuration to be built from");
        }
    }

    #[test]
    fn every_window_shares_one_webview_environment() {
        // WebView2 refuses a webview whose browser arguments differ from the environment's, and
        // the same arguments keep every window in one browser, one GPU-less renderer per site.
        let args: Vec<_> = windows().iter().map(|w| w["additionalBrowserArgs"].as_str().map(String::from)).collect();
        assert!(args.iter().all(|a| a == &args[0]), "{args:?}");
        let args = args[0].clone().unwrap();
        for flag in ["--process-per-site", "--disable-gpu", "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection"] {
            assert!(args.split(' ').any(|a| a == flag), "{flag} in {args}");
        }
        assert!(windows().iter().all(|w| w.get("dataDirectory").is_none()), "one WebView2 data folder for all");
    }
}
