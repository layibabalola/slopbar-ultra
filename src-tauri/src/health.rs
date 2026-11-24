//! The self-check behind Settings > Health and the agents' `get_health`: what SlopBar knows about
//! its own state, turned into a list of checks with a reason and the action that fixes each. It
//! is computed from what the app already has (the last snapshot, the refresh's own outcomes, the
//! journal's counters) and never contacts anything.
use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::accounts::Provider;
use crate::model::AccountUsage;
use crate::monitoring::{Renewal, RENEW_DAYS, WARN_DAYS};

/// The row that sums up the monitoring sign-ins a new sign-in fixes, when there are two or more,
/// with "Connect monitoring for all" as its fix. It repeats the accounts' own rows, so it is not
/// counted among the problems and warnings.
pub const RENEWAL_SUMMARY: &str = "monitoring_renewal";

/// The same for saved CLI sign-ins of accounts not in use that ended or end soon, with "Sign in
/// all accounts" as its fix. Not counted either.
pub const CLI_RENEWAL_SUMMARY: &str = "cli_renewal";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warning,
    Problem,
}

/// The action that fixes a check, matching one Settings already offers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Fix {
    ConnectMonitoring { provider: Provider, id: String },
    /// "Connect monitoring for all": every account that needs a monitoring sign-in, one at a time.
    ConnectMonitoringAll,
    /// "Sign in all accounts": among its sign-ins, a new CLI sign-in for every account not in use
    /// whose saved one ended or ends soon, one email at a time.
    SignInAll,
    /// "Sign in all your computers from here" (see `relay`): the same for this computer and every
    /// other one it reaches over SSH, their pages opening here, after the person's OK.
    SignInEverywhere,
    Refresh,
    RestartApp,
    OpenSettings { page: &'static str },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    /// Stable per kind ("door", "sharing") and per account ("monitoring:claude:<id>").
    pub id: String,
    pub status: Status,
    pub title: String,
    /// One line on why.
    pub reason: String,
    pub fix: Option<Fix>,
    /// How many, for checks the flyout words itself ("roster_waiting").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The worst status of any check.
    pub status: Status,
    pub problems: usize,
    pub warnings: usize,
    pub checks: Vec<Check>,
    pub checked_at: DateTime<Utc>,
}

/// What the monitor's saved session says about one account (see `Monitor::health`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MonitorState {
    pub reconnect: bool,
    /// Two renewals ended uncertain, so automatic replay stopped (`monitor_uncertain`).
    pub uncertain: bool,
    /// When the sign-in ends: the earliest end the provider gave it, at sign-in or since. Renewing
    /// never moves it later; only a new sign-in does.
    pub ends_at: Option<DateTime<Utc>>,
}

/// The agent door (see `door::serve`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Door {
    Starting,
    Listening,
    /// Opening the pipe failed; the door retries on its own.
    Retrying,
    /// Not a Windows build: there is no door.
    Unsupported,
}

/// Where in-use status is shared, and how the last refresh went with it.
#[derive(Debug, Clone, Default)]
pub struct Sharing {
    pub folder: Option<String>,
    pub remote: Option<String>,
    pub rclone_installed: bool,
    pub last: Option<SharingResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharingResult {
    pub at: DateTime<Utc>,
    pub wrote: bool,
    pub read: bool,
    /// Why the write, and the read, failed when the network's proxy stopped it
    /// (`proxy::refusal_code`: rclone was not started).
    pub write_error: Option<&'static str>,
    pub read_error: Option<&'static str>,
}

impl SharingResult {
    /// A refresh's exchange: whether this computer's record was written and the records read, and
    /// for each, the code of the network's proxy when it stopped it.
    pub fn of<W, R>(at: DateTime<Utc>, wrote: &std::io::Result<W>, read: &std::io::Result<R>) -> SharingResult {
        let code = |failed: Option<&std::io::Error>| failed.and_then(crate::proxy::refusal_code);
        SharingResult { at, wrote: wrote.is_ok(), read: read.is_ok(), write_error: code(wrote.as_ref().err()), read_error: code(read.as_ref().err()) }
    }

    /// Nothing written or read, for no known reason.
    pub fn failed(at: DateTime<Utc>) -> SharingResult {
        SharingResult { at, wrote: false, read: false, write_error: None, read_error: None }
    }
}

/// The account roster (see `roster`) and how its last sync went.
#[derive(Debug, Clone, Copy, Default)]
pub struct Roster {
    pub enabled: bool,
    pub passphrase_saved: bool,
    pub wrote: Option<bool>,
    pub read: Option<bool>,
    /// Why the last write or read failed, when the network's proxy stopped it.
    pub write_error: Option<&'static str>,
    pub read_error: Option<&'static str>,
    /// Other machines' rosters that did not open with this machine's passphrase.
    pub unreadable: usize,
    /// Accounts those rosters name that are not signed in here.
    pub waiting: usize,
    /// Rewrites in a row of this machine's roster found not as it wrote it (`roster::Seen::own_restores`).
    pub own_restores: usize,
}

/// The diagnostics journal's own counters (see `Recorder::health`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Journal {
    pub started: bool,
    pub write_attempts: u64,
    pub storage_errors: u64,
    pub last_write_ok: Option<bool>,
    pub history_unreadable: bool,
}

/// Windows notifications: whether SlopBar registered its toast identity, and whether Windows
/// lets it (and anything) show toasts. `None` is unknown.
#[derive(Debug, Clone, Copy, Default)]
pub struct Notifications {
    pub registered: Option<bool>,
    pub app_enabled: Option<bool>,
    pub system_enabled: Option<bool>,
    /// Any of SlopBar's own notification kinds is on.
    pub any_wanted: bool,
}

pub struct Inputs<'a> {
    pub accounts: &'a [AccountUsage],
    /// Keyed like `monitoring::key`.
    pub monitoring: &'a HashMap<String, MonitorState>,
    pub refresh_minutes: u32,
    /// The last refresh that read at least one account (or had none to read).
    pub last_success: Option<DateTime<Utc>>,
    pub door: Door,
    pub sharing: &'a Sharing,
    pub roster: Roster,
    pub journal: Journal,
    pub notifications: Notifications,
    /// Settings: Connect monitoring automatically.
    pub auto_connect: bool,
    /// Saved sign-ins still stored as plaintext (see `accounts::migrate_vault`).
    pub vault_plaintext: usize,
    /// SlopBar's processes in this Windows session, when they could be listed.
    pub processes: Option<crate::instance::Processes>,
    /// Space SlopBar takes on disk, when it could be measured.
    pub disk: Option<Disk>,
    /// How the last refresh went with WSL (see `sources::wsl_status`).
    pub wsl: crate::sources::WslStatus,
    /// The user's other computers this one knows (paired or checked over SSH, or in the roster):
    /// with any, the renewal summaries' fix signs them in from here too.
    pub other_computers: usize,
}

fn place_set(sharing: &Sharing) -> bool {
    sharing.remote.is_some() || sharing.folder.is_some()
}

/// For a reason to end with when the network's proxy stopped it: a space and the English sentence
/// i18n.js has for the code (what `errorText` shows), so Health and the rest of SlopBar say the
/// same. Empty for anything else.
fn proxy_reason(code: Option<&str>) -> String {
    code.filter(|c| crate::net::PROXY_CODES.contains(c)).map(|c| format!(" {}", crate::i18n::t("en", c, &[]))).unwrap_or_default()
}

/// `proxy_reason` for a write and a read that both failed: once when the proxy stopped both for
/// the same reason, else each said of what it stopped.
fn proxy_reasons(write: Option<&str>, read: Option<&str>) -> String {
    if write == read {
        return proxy_reason(write);
    }
    let of = |what: &str, code: Option<&str>| Some(proxy_reason(code)).filter(|r| !r.is_empty()).map(|r| format!(" {what}:{r}")).unwrap_or_default();
    format!("{}{}", of("Writing", write), of("Reading", read))
}

/// The longest reason a sharing row gives: get_health's strings reach agents cut at `mcp::MAX_TEXT`.
const SHARING_REASON_MAX: usize = 480;

/// `before`, `place` and `after` in one reason of at most `SHARING_REASON_MAX` characters, `place`
/// (a folder path or an rclone remote) clipped as far as it must be, so that what is said after
/// it, the network's proxy's reasons included, is never cut.
fn around(before: &str, place: &str, after: &str) -> String {
    let room = SHARING_REASON_MAX.saturating_sub(before.chars().count() + after.chars().count());
    format!("{before}{}{after}", crate::mcp::clipped(place, room.max(24)))
}

/// The account roster, while it is on: whether it can be shared, and the accounts it found that
/// wait to be signed in here.
fn roster_checks(r: &Roster, place: bool) -> Vec<Check> {
    if !r.enabled {
        return vec![check("roster", Status::Ok, "Account roster", "Off: this computer's accounts are not shared with your other computers, and theirs are not read.", None)];
    }
    let data = Some(Fix::OpenSettings { page: "data" });
    let title = "Account roster";
    let mut out = vec![if !place {
        check("roster", Status::Warning, title, "On, but in-use sharing is off: choose a folder or rclone remote under Other computers.", data)
    } else if !r.passphrase_saved {
        check("roster", Status::Warning, title, "On, but no roster passphrase is set: set the same one on each computer under Other computers.", data)
    } else {
        match (r.wrote, r.read) {
            (Some(false), _) => check("roster", Status::Problem, title,
                format!("Could not write this computer's roster, so your other computers do not learn its accounts.{}", proxy_reason(r.write_error)), data),
            (_, Some(false)) => check("roster", Status::Problem, title,
                format!("Could not read the rosters, so this computer does not learn the others' accounts.{}", proxy_reason(r.read_error)), data),
            _ if r.unreadable > 0 => check("roster", Status::Warning, title, format!("{} roster(s) did not open with this computer's passphrase: set the same passphrase on every computer.", r.unreadable), data),
            _ if r.own_restores >= crate::roster::OWN_REWRITTEN_WARN => check("roster", Status::Warning, title,
                format!("This computer's roster was found changed or gone, and written again, {} times in a row: something else writes it (another computer copied from this one, or a sync service putting back an older copy), and your other computers may see that one meanwhile.", r.own_restores), data),
            (None, None) => check("roster", Status::Ok, title, "Shared at the next refresh.", None),
            _ => check("roster", Status::Ok, title, "Shared with your other computers, encrypted.", None),
        }
    }];
    if r.waiting > 0 {
        let mut waiting = check("roster_waiting", Status::Warning, format!("{} accounts from your other machines are waiting to be signed in", r.waiting),
            "Sign them in here with Sign in all accounts (Settings > Accounts).", Some(Fix::OpenSettings { page: "accounts" }));
        waiting.count = Some(r.waiting);
        out.push(waiting);
    }
    out
}

/// WSL, when it is there or was asked about: which distros are read, which are left stopped,
/// and whether WSL stopped answering. None on a PC without WSL.
fn wsl_check(s: &crate::sources::WslStatus) -> Option<Check> {
    use crate::sources::{WslPlan, WslScan};
    let data = Some(Fix::OpenSettings { page: "data" });
    s.checked_at?;
    // Backing off after a single slow listing is not a warning: `timed_out` is set only once
    // listings keep missing (see `sources::listing_stuck`) or a distro did not answer.
    if s.timed_out {
        let slow: Vec<&str> = s.distros.iter().filter(|d| d.timed_out).map(|d| d.name.as_str()).collect();
        let what = if slow.is_empty() { "WSL did not answer in time".to_string() } else { format!("WSL ({}) did not answer in time", crate::mcp::listed(&slow)) };
        return Some(check("wsl", Status::Warning, "WSL",
            format!("{what}, so SlopBar skipped it rather than wait; it tries again in a few minutes. If WSL stays stuck, `wsl --shutdown` in a terminal restarts it, or set Look for accounts in WSL to Off (Settings > Data)."), data));
    }
    if !s.installed { return None; }
    if s.mode == WslScan::Off {
        return Some(check("wsl", Status::Ok, "WSL", "Not searched for accounts: Look for accounts in WSL is Off (Settings > Data).", None));
    }
    let names = |f: &dyn Fn(&crate::sources::DistroStatus) -> bool| s.distros.iter().filter(|d| f(d)).map(|d| d.name.clone()).collect::<Vec<_>>();
    let read = names(&|d| d.plan != WslPlan::Skip);
    let skipped = names(&|d| d.plan == WslPlan::Skip);
    let mut parts = vec![];
    // Distro names are listed in a room of their own: get_health's strings reach agents cut at
    // `mcp::MAX_TEXT`, which must never cut the advice after them.
    if !read.is_empty() { parts.push(format!("Read: {}.", crate::mcp::listed(&read))); }
    if !skipped.is_empty() {
        parts.push(format!("Left stopped: {} (no CLI sign-ins seen there; Rescan WSL in Settings > Data looks once).", crate::mcp::listed(&skipped)));
    }
    if parts.is_empty() { parts.push("No distros.".into()); }
    Some(check("wsl", Status::Ok, "WSL", parts.join(" "), None))
}

/// What SlopBar keeps on disk: its app data (diagnostics exports and captures included) and the
/// rollback copies local installs leave (`scripts/install-local-build.ps1`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Disk {
    pub app_data: u64,
    pub diagnostics: u64,
    pub rollback: u64,
    pub rollback_folders: usize,
}

impl Disk {
    /// Measures the app data folder and the rollback folder.
    pub fn measure(app_data: &std::path::Path, rollback: &std::path::Path) -> Self {
        let folders = std::fs::read_dir(rollback).map(|d| d.flatten().filter(|e| e.path().is_dir()).count()).unwrap_or(0);
        Disk { app_data: size(app_data), diagnostics: size(&app_data.join("diagnostics")), rollback: size(rollback), rollback_folders: folders }
    }
}

/// Bytes in the files under `path` (symbolic links are not followed).
pub fn size(path: &std::path::Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    std::fs::read_dir(path).map(|d| d.flatten().map(|e| size(&e.path())).sum()).unwrap_or(0)
}

/// "1.2 GB", "683 MB", "4 KB".
pub fn bytes(n: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let n = n as f64;
    if n >= 1024.0 * MB { format!("{:.1} GB", n / (1024.0 * MB)) }
    else if n >= MB { format!("{:.0} MB", n / MB) }
    else { format!("{:.0} KB", (n / 1024.0).ceil()) }
}

fn name(a: &AccountUsage) -> String {
    a.alias.clone().or_else(|| a.email.clone()).unwrap_or_else(|| format!("{} account", label(a.provider)))
}

fn label(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
    }
}

fn ago(now: DateTime<Utc>, at: DateTime<Utc>) -> String {
    let minutes = (now - at).num_minutes().max(0);
    match minutes {
        0 => "less than a minute ago".into(),
        1 => "1 minute ago".into(),
        m if m < 120 => format!("{m} minutes ago"),
        m if m < 48 * 60 => format!("{} hours ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

fn check(id: impl Into<String>, status: Status, title: impl Into<String>, reason: impl Into<String>, fix: Option<Fix>) -> Check {
    Check { id: id.into(), status, title: title.into(), reason: reason.into(), fix, count: None }
}

/// Why a sign-in ends however often it is renewed, for the provider that does that.
fn sign_in_limit(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "Claude ends every sign-in about 30 days after it is made, and renewing cannot extend it.",
        Provider::Codex => "The provider ends each sign-in on the date it set when it was made, and renewing cannot extend it.",
    }
}

/// "within 5 hours" or "in 3 days": how soon something `left` from now happens.
fn within(left: Duration) -> String {
    let hours = left.num_hours().max(1);
    match hours {
        1 => "within 1 hour".into(),
        h if h < 48 => format!("within {h} hours"),
        h => format!("in {} days", h / 24),
    }
}

/// The monitoring sign-ins a new sign-in fixes, by kind (see `RENEWAL_SUMMARY`).
#[derive(Default)]
struct SignIns {
    ended: usize,
    refused: usize,
    ending: usize,
    unconnected: usize,
    /// What "Connect monitoring for all" would sign in: these, and those ending within `RENEW_DAYS`.
    queued: usize,
    /// Of those with a row of their own, the ones another computer's recent reading covers
    /// (`covered_by`).
    covered: usize,
    /// Ended or refused with no other computer covering them: what makes the summary a problem.
    faults: usize,
    /// The provider whose fixed limit ended one of them, or ends one soon.
    limited: Option<Provider>,
}

/// A renewal summary's reason and fix once the user has other computers: "Sign in all your
/// computers from here" renews theirs in the same sitting, their pages opening here (`relay`).
/// One short sentence: get_health's strings reach agents cut at `mcp::MAX_TEXT`, and the CLI
/// summary is long already.
fn everywhere(reason: String, fix: Fix, other_computers: usize) -> (String, Fix) {
    if other_computers == 0 || !matches!(fix, Fix::SignInAll | Fix::ConnectMonitoringAll) {
        return (reason, fix);
    }
    let others = if other_computers == 1 { "your other computer's".to_string() } else { format!("your {other_computers} other computers'") };
    (format!("{reason} Sign in from here adds {others} sign-ins, their pages opening here, after you confirm."), Fix::SignInEverywhere)
}

/// Whether "Sign in all accounts" makes the account a new CLI sign-in, by `now`'s dates as
/// `bulk::needs_cli_sign_in` goes by the snapshot's: its saved one ended or ends within
/// `RENEW_DAYS`, or the provider rejected it, and it is not in use; never a token-only account.
fn cli_due(a: &AccountUsage, now: DateTime<Utc>) -> bool {
    (crate::accounts::cli_renewal(a.cli_ends_at, a.active, now).is_some() || crate::bulk::rejected_claude_login(a)) && !a.active && !a.id.starts_with("token:")
}

/// Saved CLI sign-ins that ended, or end within `WARN_DAYS` (see `accounts::cli_renewal`): one row
/// each, and one more for two or more that "Sign in all accounts" can make again
/// (`CLI_RENEWAL_SUMMARY`). An account not in use is only a warning: nothing fails until the CLI is
/// switched to it, and automatic switching never is. The account in use is a problem once its
/// sign-in ended, since the CLI stops (an agent running it never sees the CLI's own notice), but
/// SlopBar never signs it in: the CLI manages that sign-in.
fn cli_sign_ins(accounts: &[AccountUsage], other_computers: usize, now: DateTime<Utc>) -> Vec<Check> {
    use crate::model::RenewalKind;
    let mut rows = vec![];
    let (mut ended, mut ending) = (0, 0);
    let mut limited = None;
    for a in accounts {
        let Some(at) = a.cli_ends_at else { continue };
        let id = format!("cli_signin:{}:{}", a.provider.key(), a.id);
        if a.active {
            let cli = match a.provider { Provider::Claude => "claude /login", Provider::Codex => "codex login" };
            if at <= now {
                rows.push(check(id, Status::Problem, format!("{}: CLI sign-in ended", name(a)),
                    format!("{} The CLI's own sign-in, in use now, ended {}: the CLI stops until it signs in again. Run `{cli}`, or Use another account; the CLI manages this sign-in, so SlopBar leaves it alone.",
                        sign_in_limit(a.provider), ago(now, at)), None));
            } else if at <= now + Duration::days(WARN_DAYS) {
                rows.push(check(id, Status::Warning, format!("{}: CLI sign-in ends soon", name(a)),
                    format!("{} The CLI's own sign-in, in use now, ends {}; run `{cli}` before then, or the CLI stops until it signs in again.",
                        sign_in_limit(a.provider), within(at - now)), None));
            }
            continue;
        }
        let Some(kind) = crate::accounts::cli_renewal(Some(at), false, now) else { continue };
        // As Sign in all accounts counts them (`bulk::needs_cli_sign_in`): never a token-only account.
        let fixable = !a.id.starts_with("token:");
        let (how, fix) = if fixable {
            ("Sign in all accounts signs it in again: one browser sign-in as this account, saved as its CLI sign-in. It switches nothing and signs nothing out.", Fix::SignInAll)
        } else {
            ("SlopBar cannot sign it in again, since it does not know which account it is: switch to it and sign the CLI in, or remove it if it is no longer used.",
                Fix::OpenSettings { page: "accounts" })
        };
        if kind == RenewalKind::Ended {
            if fixable {
                ended += 1;
                limited = limited.or(Some(a.provider));
            }
            rows.push(check(id, Status::Warning, format!("{}: CLI sign-in ended", name(a)),
                format!("{} Its saved CLI sign-in ended {}, so switching the CLI to it would stop the CLI until it signs in again; automatic switching passes it over. {how}",
                    sign_in_limit(a.provider), ago(now, at)), Some(fix)));
        } else if at <= now + Duration::days(WARN_DAYS) {
            if fixable {
                ending += 1;
                limited = limited.or(Some(a.provider));
            }
            rows.push(check(id, Status::Warning, format!("{}: CLI sign-in ends soon", name(a)),
                format!("{} Its saved CLI sign-in ends {}; after that, switching the CLI to it would stop the CLI until it signs in again, so automatic switching passes it over already. {how}",
                    sign_in_limit(a.provider), within(at - now)), Some(fix)));
        }
        // Ending within RENEW_DAYS only: Sign in all accounts takes it along; no row of its own yet.
    }
    if ended + ending >= 2 {
        let queued = accounts.iter().filter(|a| cli_due(a, now)).count();
        let mut parts = vec![];
        if ended > 0 { parts.push(format!("{ended} ended")); }
        if ending > 0 { parts.push(format!("{ending} {} soon", if ending == 1 { "ends" } else { "end" })); }
        let reason = format!("{} No setting makes it permanent. Sign in all accounts signs in the {queued} accounts not in use that need it one after another, including any ending within {RENEW_DAYS} days, so they next end together. It switches nothing, and with monitoring on it renews their monitoring sign-ins in the same sitting.",
            limited.map(sign_in_limit).unwrap_or_default());
        let (reason, fix) = everywhere(reason, Fix::SignInAll, other_computers);
        let mut summary = check(CLI_RENEWAL_SUMMARY, Status::Warning, format!("CLI sign-ins: {}", parts.join(", ")), reason, Some(fix));
        summary.count = Some(queued);
        rows.insert(0, summary);
    }
    rows
}

/// Every check, worst first within the order Settings lists them.
pub fn checks(inputs: &Inputs, now: DateTime<Utc>) -> Report {
    let interval = Duration::minutes(inputs.refresh_minutes.max(1) as i64);
    // Two refresh intervals, plus slack for a slow poll.
    let max_age = interval * 2 + Duration::seconds(60);
    let mut checks = vec![];

    // Monitoring sign-ins, one row per account that needs something; a single row otherwise.
    let mut monitoring = vec![];
    let mut sign_ins = SignIns::default();
    // Accounts whose monitoring row already says why their reading is old (see the freshness rows).
    let mut explained = std::collections::HashSet::new();
    for a in inputs.accounts {
        // Monitor sessions are keyed like `monitoring::key`; check ids use the lowercase provider key.
        let key = format!("{}:{}", a.provider.name(), a.id);
        let id = format!("monitoring:{}:{}", a.provider.key(), a.id);
        let who = name(a);
        let connect = Some(Fix::ConnectMonitoring { provider: a.provider, id: a.id.clone() });
        // Another of the user's computers read it recently (the shared account roster): its
        // readings show here, so this computer's own sign-in matters less.
        // Short: with the sign-in limit and the advice to reconnect, it makes the longest row an
        // agent reads, cut at `mcp::MAX_TEXT` (see `every_reason_reaches_an_agent_whole`).
        let covered = a.covered_by.as_deref().map(|m| format!(
            "{m} still reads this account's usage, which shows here through your computers' shared account list."));
        if !a.monitor_connected {
            let how = if inputs.auto_connect {
                "Connect monitoring automatically is on (Settings > Data): SlopBar starts its sign-in once it has been in use for 3 minutes, or connect it now."
            } else {
                "Connect monitoring automatically is off (Settings > Data): connect it now, or turn that on to have SlopBar start the sign-in when it is in use."
            };
            let meanwhile = covered.as_deref().map(|c| format!(" {c}")).unwrap_or_default();
            monitoring.push(check(id, Status::Warning, format!("{who}: monitoring not connected"),
                format!("Its usage is only read here while it is the account in use.{meanwhile} {how}"), connect));
            // As Connect all counts them (`bulk::needs_sign_in`): never a token-only account.
            if !a.id.starts_with("token:") {
                sign_ins.unconnected += 1;
                sign_ins.queued += 1;
                sign_ins.covered += usize::from(covered.is_some());
            }
            continue;
        }
        let state = inputs.monitoring.get(&key);
        let error = a.monitor_error.as_deref();
        // An account not in use carries its monitoring error as its error.
        let renewal = crate::monitoring::renewal(state, error.or(a.error.as_deref()), now);
        if renewal.due() {
            sign_ins.queued += 1;
        }
        let ending = match renewal { Renewal::Ending(at) if at <= now + Duration::days(WARN_DAYS) => Some(at), _ => None };
        // Ended or refused here but covered: a warning, since this computer's switching and alerts
        // still go by its own readings alone.
        let (status, reconnect) = match &covered {
            Some(c) => (Status::Warning, format!("{c} Automatic switching and alerts here go by this computer's own readings, so reconnect: one browser sign-in as this account that switches nothing and needs no account use.")),
            None => {
                sign_ins.faults += usize::from(matches!(renewal, Renewal::Ended(_) | Renewal::Refused));
                (Status::Problem, "Reconnecting is one browser sign-in as this account; it switches nothing and needs no account use.".into())
            }
        };
        if let Renewal::Ended(at) = renewal {
            sign_ins.ended += 1;
            sign_ins.covered += usize::from(covered.is_some());
            sign_ins.limited = sign_ins.limited.or(Some(a.provider));
            explained.insert(key);
            let when = if at <= now { format!("ended {}", ago(now, at)) }
                else { "ended a little early, which the provider may do".into() };
            monitoring.push(check(id, status, format!("{who}: monitoring sign-in ended"),
                format!("{} This one {when}. {reconnect}", sign_in_limit(a.provider)), connect));
        } else if renewal == Renewal::Refused {
            sign_ins.refused += 1;
            sign_ins.covered += usize::from(covered.is_some());
            explained.insert(key);
            let early = if state.and_then(|s| s.ends_at).is_some_and(|t| t > now) { ", well before the date it was due to end" } else { "" };
            monitoring.push(check(id, status, format!("{who}: monitoring sign-in needs reconnecting"),
                format!("Its monitoring sign-in can no longer be renewed{early}. {reconnect}"), connect));
        } else if error == Some("monitor_storage") {
            monitoring.push(check(id, Status::Problem, format!("{who}: monitoring sign-in unreadable"),
                "The saved monitoring sign-in could not be read or saved. Check disk space and file access, then reconnect.", connect));
        } else if let Some(at) = ending {
            sign_ins.ending += 1;
            sign_ins.covered += usize::from(covered.is_some());
            sign_ins.limited = sign_ins.limited.or(Some(a.provider));
            monitoring.push(check(id, Status::Warning, format!("{who}: monitoring sign-in ends soon"),
                format!("{} This one ends {}; reconnect before then to avoid a gap.", sign_in_limit(a.provider), within(at - now)), connect));
        } else if let Some(e) = error {
            // The wait (`Monitor::token_with`), during which the same reason is kept without asking
            // again; for the proxy, also the site, since a proxy may let usage through but not it.
            let why = if e == "monitor_network" { "The monitoring service could not be reached; SlopBar tries again at the first refresh at least 5 minutes after it failed.".to_string() }
                else if crate::net::PROXY_CODES.contains(&e) {
                    format!("Monitoring could not renew its sign-in at {}.{} SlopBar tries the renewal again at the first refresh at least 5 minutes after it failed, or when it starts again.",
                        crate::monitoring::token_host(a.provider), proxy_reason(Some(e)))
                }
                else if e.starts_with("rate_limited") { "The provider is rate-limiting monitoring; it is retried later.".to_string() }
                else { format!("Monitoring failed ({}); it is retried at each refresh.", e.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(32).collect::<String>()) };
            monitoring.push(check(id, Status::Warning, format!("{who}: monitoring failing"), why, Some(Fix::Refresh)));
        }
    }
    // Two or more of those: one row to sign them all in again, in one sitting.
    let fixable = sign_ins.ended + sign_ins.refused + sign_ins.ending + sign_ins.unconnected;
    if fixable >= 2 {
        let mut parts = vec![];
        if sign_ins.ended > 0 { parts.push(format!("{} ended", sign_ins.ended)); }
        if sign_ins.refused > 0 { parts.push(format!("{} {} reconnecting", sign_ins.refused, if sign_ins.refused == 1 { "needs" } else { "need" })); }
        if sign_ins.ending > 0 { parts.push(format!("{} {} soon", sign_ins.ending, if sign_ins.ending == 1 { "ends" } else { "end" })); }
        if sign_ins.unconnected > 0 { parts.push(format!("{} not connected", sign_ins.unconnected)); }
        // A problem only while one that ended or was refused has no other computer covering it.
        let status = if sign_ins.faults > 0 { Status::Problem } else { Status::Warning };
        let covered = match sign_ins.covered {
            0 => String::new(),
            n => format!(" ({n} covered by your other computers' readings)"),
        };
        let mut reason = if let Some(p) = sign_ins.limited {
            format!("{} No setting makes it permanent. Connect all signs in the {} accounts that need it one after another, including any ending within {RENEW_DAYS} days, so they next end together.",
                sign_in_limit(p), sign_ins.queued)
        } else {
            format!("Connect all signs in the {} accounts that need it one after another.", sign_ins.queued)
        };
        // Saved CLI sign-ins due as well: Sign in all accounts makes both in one sitting, an
        // email's in one browser window, where Connect all would leave the CLI ones for another.
        let cli_count = inputs.accounts.iter().filter(|a| cli_due(a, now)).count();
        let fix = if cli_count > 0 {
            reason.push_str(&format!(" Sign in all accounts does that and makes the {cli_count} saved CLI sign-in{} due in the same sitting.", if cli_count == 1 { "" } else { "s" }));
            Fix::SignInAll
        } else {
            Fix::ConnectMonitoringAll
        };
        let (reason, fix) = everywhere(reason, fix, inputs.other_computers);
        let mut summary = check(RENEWAL_SUMMARY, status, format!("Monitoring sign-ins: {}{covered}", parts.join(", ")), reason, Some(fix));
        summary.count = Some(sign_ins.queued);
        monitoring.insert(0, summary);
    }
    if monitoring.is_empty() {
        let reason = if inputs.accounts.is_empty() { "No accounts yet.".to_string() } else { format!("All {} accounts have working monitoring sign-ins.", inputs.accounts.len()) };
        checks.push(check("monitoring", Status::Ok, "Monitoring sign-ins", reason, None));
    }
    checks.extend(monitoring);

    // The CLI's own saved login, which SlopBar switches but never renews. The CLI signs in again
    // the one in use; Sign in all accounts makes a new one for an account not in use.
    // Not in use, it is a warning, as a saved sign-in that ended is: nothing fails until a switch.
    for a in inputs.accounts.iter().filter(|a| a.cli_rejected) {
        let cli = match a.provider { Provider::Claude => "claude /login", Provider::Codex => "codex login" };
        let (status, how, fix) = if a.active {
            (Status::Problem, format!("Run `{cli}` to sign the CLI in again; the CLI manages that sign-in."), Fix::OpenSettings { page: "accounts" })
        } else if crate::bulk::rejected_claude_login(a) && !a.id.starts_with("token:") {
            (Status::Warning, "Sign in all accounts signs it in again as a new CLI sign-in for this account, switching nothing; or remove it if it is no longer used.".to_string(), Fix::SignInAll)
        } else {
            (Status::Warning, format!("Switch to it and run `{cli}`, or remove it if it is no longer used."), Fix::OpenSettings { page: "accounts" })
        };
        checks.push(check(format!("cli:{}:{}", a.provider.key(), a.id), status, format!("{}: CLI sign-in rejected", name(a)),
            format!("The provider no longer accepts the CLI's saved sign-in for this account. {how}"), Some(fix)));
    }
    checks.extend(cli_sign_ins(inputs.accounts, inputs.other_computers, now));
    if inputs.vault_plaintext > 0 {
        checks.push(check("vault", Status::Warning, "Saved sign-ins",
            format!("{} saved sign-in(s) could not be encrypted yet and are still stored as plaintext; SlopBar retries at each start. Check disk space and file access.", inputs.vault_plaintext),
            Some(Fix::RestartApp)));
    }

    // Readings older than two refresh intervals. An unmonitored account not in use is already
    // reported above: its reading cannot be fresh until monitoring is connected. Nor can one whose
    // monitoring sign-in ended or was refused, so a refresh would not help it either.
    let mut stale = vec![];
    for a in inputs.accounts.iter().filter(|a| a.monitor_connected || a.active) {
        let old = a.fetched_at.map_or(true, |t| now - t > max_age);
        if !(a.stale || old) || inputs.last_success.is_none() {
            continue;
        }
        if !a.active && explained.contains(&format!("{}:{}", a.provider.name(), a.id)) {
            continue;
        }
        let who = name(a);
        let reason = match a.fetched_at {
            Some(t) => format!("Last confirmed {}, more than two refresh intervals ago.", ago(now, t)),
            None => "No usage has been read for it yet.".into(),
        };
        // The network's proxy, when it stopped the read or monitoring's renewal before it.
        let reason = format!("{reason}{}", proxy_reason(a.error.as_deref()));
        stale.push(check(format!("freshness:{}:{}", a.provider.key(), a.id), Status::Warning, format!("{who}: usage is out of date"), reason, Some(Fix::Refresh)));
    }
    if stale.is_empty() {
        checks.push(check("freshness", Status::Ok, "Usage readings", "Every monitored account's reading is current.", None));
    }
    checks.extend(stale);

    // When the network's proxy stops every account's read for the same reason (from the start,
    // say), that is why nothing was read: the rows above have nothing to say before a first read.
    let blocked = inputs.accounts.first().and_then(|a| a.error.as_deref())
        .filter(|code| inputs.accounts.iter().all(|a| a.error.as_deref() == Some(*code)));
    checks.push(match inputs.last_success {
        None => check("refresh", Status::Warning, "Last refresh", format!("No refresh has read any account yet.{}", proxy_reason(blocked)), Some(Fix::Refresh)),
        Some(t) if now - t > max_age => check("refresh", Status::Problem, "Last refresh",
            format!("The last refresh that read usage was {}; refreshes run every {} minutes.{}", ago(now, t), interval.num_minutes(), proxy_reason(blocked)), Some(Fix::Refresh)),
        Some(t) => check("refresh", Status::Ok, "Last refresh", format!("Usage was read {}.", ago(now, t)), None),
    });

    if let Some(p) = inputs.processes {
        checks.push(if p.instances > 1 {
            check("instances", Status::Problem, "SlopBar is running more than once",
                format!("{} copies are running and each renews the same monitoring sign-ins, which can get them rejected. Restarting leaves one.", p.instances),
                Some(Fix::RestartApp))
        } else {
            check("instances", Status::Ok, "Running once", "One copy of SlopBar is running.", None)
        });
        // Information only: each agent session starts its own server, which ends with the session.
        checks.push(check("agent_servers", Status::Ok, "Agent servers", match p.mcp_servers {
            0 => "No agent is connected (slopbar-mcp).".to_string(),
            1 => "1 agent server (slopbar-mcp) is running.".to_string(),
            n => format!("{n} agent servers (slopbar-mcp) are running, one per agent session."),
        }, None));
    }

    // Information only: exports, captures and rollbacks are pruned on their own.
    if let Some(d) = inputs.disk {
        let rollback = match d.rollback_folders {
            0 => "no install rollbacks".to_string(),
            1 => format!("1 install rollback, {}", bytes(d.rollback)),
            n => format!("{n} install rollbacks, {}", bytes(d.rollback)),
        };
        checks.push(check("disk", Status::Ok, "Disk use",
            format!("App data {} (diagnostics {}); {rollback}.", bytes(d.app_data), bytes(d.diagnostics)), None));
    }

    match inputs.door {
        Door::Unsupported => {}
        Door::Listening => checks.push(check("door", Status::Ok, "Agent door", "Listening for agents (slopbar-mcp).", None)),
        Door::Starting => checks.push(check("door", Status::Warning, "Agent door", "Still opening; agents can connect shortly.", None)),
        Door::Retrying => checks.push(check("door", Status::Problem, "Agent door",
            "Agents cannot reach SlopBar: the door could not be opened (another copy may still be exiting). Restarting reopens it.", Some(Fix::RestartApp))),
    }

    let sharing = inputs.sharing;
    let data = Some(Fix::OpenSettings { page: "data" });
    if let Some(remote) = &sharing.remote {
        checks.push(if sharing.rclone_installed {
            check("rclone", Status::Ok, "rclone", format!("Installed, for the remote {remote}."), None)
        } else {
            check("rclone", Status::Problem, "rclone", format!("Sharing uses the rclone remote {remote}, but rclone is not installed, so nothing is shared."), data.clone())
        });
    }
    let place = match (&sharing.remote, &sharing.folder) {
        (Some(r), _) => Some(format!("the rclone remote {r}")),
        (None, Some(f)) => Some(format!("the folder {f}")),
        (None, None) => None,
    };
    checks.push(match (place, sharing.last) {
        (None, _) => check("sharing", Status::Ok, "In-use sharing", "Off: in-use status is not shared with other computers.", None),
        (Some(_), _) if sharing.remote.is_some() && !sharing.rclone_installed => check("sharing", Status::Problem, "In-use sharing",
            "Not running: rclone is missing.", data),
        (Some(p), None) => check("sharing", Status::Ok, "In-use sharing", around("Shares through ", &p, "; checked at the next refresh."), None),
        (Some(p), Some(r)) => match (r.wrote, r.read) {
            (true, true) => check("sharing", Status::Ok, "In-use sharing", around("Shared through ", &p, &format!(" {}.", ago(now, r.at))), None),
            (false, true) => check("sharing", Status::Problem, "In-use sharing",
                around("Could not write to ", &p, &format!(", so other computers do not see this one.{}", proxy_reason(r.write_error))), data),
            (true, false) => check("sharing", Status::Problem, "In-use sharing",
                around("Could not read ", &p, &format!(", so this computer does not see the others.{}", proxy_reason(r.read_error))), data),
            (false, false) => check("sharing", Status::Problem, "In-use sharing",
                around("Could not write to or read ", &p, &format!(".{}", proxy_reasons(r.write_error, r.read_error))), data),
        },
    });

    // One account on several computers at once (seen through sharing, live ones only): a planned
    // share when its busiest recent weeks left room for them all (`AccountUsage::room_to_share`),
    // a conflict otherwise.
    for a in inputs.accounts.iter().filter(|a| a.overlap.len() > 1) {
        let n = a.overlap.len();
        let (id, list) = (format!("overlap:{}:{}", a.provider.key(), a.id), crate::mcp::listed(&a.overlap));
        checks.push(if a.room_to_share {
            check(id, Status::Ok, format!("{}: shared by {n} computers", name(a)),
                format!("In use on {list} at the same time. In its busiest recent weeks it stayed under its weekly limits, so these computers' use adds up within them; the 5-hour limit is shared too."),
                None)
        } else {
            check(id, Status::Warning, format!("{}: in use on {n} computers at once", name(a)),
                format!("In use on {list} at the same time: its limits drain about {n}x as fast. SlopBar has not seen it stay under its weekly limits in recent weeks, so this is not a planned share. Switch one of those computers to another account (recommend_account names a free one)."),
                Some(Fix::OpenSettings { page: "accounts" }))
        });
    }

    // The CLI and the desktop app on this machine on different accounts (one check per app).
    let mut drifted = vec![];
    for a in inputs.accounts.iter().filter(|a| a.active && !a.desktop) {
        let Some(d) = &a.seats else { continue };
        if drifted.contains(&a.provider) {
            continue;
        }
        drifted.push(a.provider);
        // Each name clipped, so the advice after them reaches an agent (`mcp::MAX_TEXT`).
        let desktop = d.desktop.as_deref().map_or_else(|| "an account SlopBar does not have".into(), |n| crate::mcp::clipped(n, crate::mcp::NAME_ROOM));
        checks.push(check(format!("desktop_drift:{}", a.provider.key()), Status::Warning,
            format!("{} and the CLI use different accounts", d.app),
            format!("CLI: {} · Desktop: {desktop}. Sign {} in with the CLI's account from its own account menu, or use another account with \"Use also switches the desktop app\" on to move both.",
                crate::mcp::clipped(d.cli.as_deref().unwrap_or_default(), crate::mcp::NAME_ROOM), d.app),
            Some(Fix::OpenSettings { page: "accounts" })));
    }

    checks.extend(roster_checks(&inputs.roster, place_set(sharing)));

    checks.extend(wsl_check(&inputs.wsl));

    let j = inputs.journal;
    checks.push(if !j.started {
        check("journal", Status::Warning, "Diagnostics journal", "Not started yet.", None)
    } else if j.last_write_ok == Some(false) {
        check("journal", Status::Problem, "Diagnostics journal",
            format!("Could not be saved ({} of {} writes failed). Check disk space and file access; monitoring is unaffected.", j.storage_errors, j.write_attempts), None)
    } else if j.history_unreadable {
        check("journal", Status::Warning, "Diagnostics journal", "Earlier history could not be read and was started afresh.", None)
    } else if j.write_attempts == 0 {
        check("journal", Status::Ok, "Diagnostics journal", "Writable; nothing needed saving yet.", None)
    } else {
        check("journal", Status::Ok, "Diagnostics journal", "Saved normally.", None)
    });

    let n = inputs.notifications;
    let open = Some(Fix::OpenSettings { page: "notifications" });
    checks.push(if !n.any_wanted {
        check("notifications", Status::Ok, "Notifications", "Every SlopBar notification is turned off.", open)
    } else if n.system_enabled == Some(false) {
        check("notifications", Status::Warning, "Notifications", "Windows notifications are turned off for every app (Windows Settings > System > Notifications).", None)
    } else if n.app_enabled == Some(false) {
        check("notifications", Status::Warning, "Notifications", "Windows notifications are turned off for SlopBar (Windows Settings > System > Notifications).", None)
    } else if n.registered == Some(false) {
        check("notifications", Status::Warning, "Notifications", "SlopBar could not register its notification name and icon; restarting retries.", Some(Fix::RestartApp))
    } else {
        check("notifications", Status::Ok, "Notifications", "Registered with Windows.", None)
    });

    // The summaries repeat rows already counted.
    let counted = || checks.iter().filter(|c| c.id != RENEWAL_SUMMARY && c.id != CLI_RENEWAL_SUMMARY);
    let problems = counted().filter(|c| c.status == Status::Problem).count();
    let warnings = counted().filter(|c| c.status == Status::Warning).count();
    let status = checks.iter().map(|c| c.status).max().unwrap_or(Status::Ok);
    Report { status, problems, warnings, checks, checked_at: now }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 21, 12, 0, 0).unwrap()
    }

    fn account(id: &str, monitored: bool, fetched_minutes_ago: Option<i64>) -> AccountUsage {
        serde_json::from_value(serde_json::json!({
            "id": id, "provider": "claude", "email": format!("{id}@example.invalid"), "alias": null, "plan": "max", "org": null,
            "active": false, "sources": [], "windows": [], "credits": null, "resets": [], "error": null,
            "stale": false, "fetched_at": fetched_minutes_ago.map(|m| now() - Duration::minutes(m)), "monitor_connected": monitored,
        })).unwrap()
    }

    struct Case {
        accounts: Vec<AccountUsage>,
        monitoring: HashMap<String, MonitorState>,
        last_success: Option<DateTime<Utc>>,
        door: Door,
        sharing: Sharing,
        roster: Roster,
        journal: Journal,
        notifications: Notifications,
        auto_connect: bool,
        vault_plaintext: usize,
        processes: Option<crate::instance::Processes>,
        disk: Option<Disk>,
        wsl: crate::sources::WslStatus,
        other_computers: usize,
    }

    impl Case {
        fn healthy() -> Self {
            Case {
                accounts: vec![account("a", true, Some(1)), account("b", true, Some(2))],
                monitoring: HashMap::new(),
                last_success: Some(now() - Duration::minutes(1)),
                door: Door::Listening,
                sharing: Sharing::default(),
                roster: Roster::default(),
                journal: Journal { started: true, write_attempts: 4, storage_errors: 0, last_write_ok: Some(true), history_unreadable: false },
                notifications: Notifications { registered: Some(true), app_enabled: None, system_enabled: None, any_wanted: true },
                auto_connect: true,
                vault_plaintext: 0,
                processes: Some(crate::instance::Processes { instances: 1, mcp_servers: 3 }),
                disk: None,
                wsl: Default::default(),
                other_computers: 0,
            }
        }

        fn run(&self) -> Report {
            checks(&Inputs { accounts: &self.accounts, monitoring: &self.monitoring, refresh_minutes: 5, last_success: self.last_success,
                door: self.door, sharing: &self.sharing, roster: self.roster, journal: self.journal, notifications: self.notifications, auto_connect: self.auto_connect,
                vault_plaintext: self.vault_plaintext, processes: self.processes, disk: self.disk, wsl: self.wsl.clone(), other_computers: self.other_computers }, now())
        }
    }

    fn find<'a>(r: &'a Report, id: &str) -> &'a Check {
        r.checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no {id} in {:?}", r.checks.iter().map(|c| &c.id).collect::<Vec<_>>()))
    }

    #[test]
    fn a_healthy_machine_is_all_ok() {
        let r = Case::healthy().run();
        assert_eq!((r.status, r.problems, r.warnings), (Status::Ok, 0, 0), "{:#?}", r.checks);
        let ids: Vec<_> = r.checks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["monitoring", "freshness", "refresh", "instances", "agent_servers", "door", "sharing", "roster", "journal", "notifications"]);
    }

    #[test]
    fn disk_use_is_information_with_app_data_and_rollback_sizes() {
        let mut case = Case::healthy();
        case.disk = Some(Disk { app_data: 24 * 1024 * 1024, diagnostics: 12 * 1024 * 1024, rollback: 61 * 1024 * 1024, rollback_folders: 3 });
        let r = case.run();
        let d = find(&r, "disk");
        assert_eq!((d.status, d.fix.clone()), (Status::Ok, None));
        assert_eq!(d.reason, "App data 24 MB (diagnostics 12 MB); 3 install rollbacks, 61 MB.");
        case.disk = Some(Disk { app_data: 3000, ..Disk::default() });
        assert_eq!(find(&case.run(), "disk").reason, "App data 3 KB (diagnostics 0 KB); no install rollbacks.");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
    }

    #[test]
    fn disk_use_is_measured_from_the_files() {
        let dir = crate::test_support::TempDir::new();
        let data = dir.0.join("data");
        std::fs::create_dir_all(data.join("diagnostics/exports")).unwrap();
        std::fs::write(data.join("settings.json"), vec![0u8; 100]).unwrap();
        std::fs::write(data.join("diagnostics/exports/a.json"), vec![0u8; 400]).unwrap();
        let rollback = dir.0.join("rollback");
        for name in ["20261001-100000", "20261002-100000"] {
            std::fs::create_dir_all(rollback.join(name)).unwrap();
            std::fs::write(rollback.join(name).join("slopbar.exe"), vec![0u8; 1000]).unwrap();
        }
        assert_eq!(Disk::measure(&data, &rollback), Disk { app_data: 500, diagnostics: 400, rollback: 2000, rollback_folders: 2 });
        assert_eq!(Disk::measure(&dir.0.join("absent"), &dir.0.join("absent")), Disk::default());
    }

    #[test]
    fn a_second_copy_is_a_problem_and_agent_servers_are_only_information() {
        let mut case = Case::healthy();
        case.processes = Some(crate::instance::Processes { instances: 2, mcp_servers: 25 });
        let r = case.run();
        let c = find(&r, "instances");
        assert_eq!((c.status, c.fix.clone(), c.title.as_str()), (Status::Problem, Some(Fix::RestartApp), "SlopBar is running more than once"));
        let servers = find(&r, "agent_servers");
        assert_eq!((servers.status, servers.fix.clone()), (Status::Ok, None));
        assert!(servers.reason.starts_with("25 agent servers"));
        // Unknown (the list could not be read): no claim either way.
        case.processes = None;
        let r = case.run();
        assert!(!r.checks.iter().any(|c| c.id == "instances" || c.id == "agent_servers"));
    }

    #[test]
    fn monitoring_that_needs_reconnecting_is_a_problem_with_its_fix() {
        let mut case = Case::healthy();
        case.accounts[0].monitor_error = Some("monitor_reconnect".into());
        case.monitoring.insert("Claude:b".into(), MonitorState { reconnect: true, uncertain: false, ends_at: None });
        let r = case.run();
        for id in ["a", "b"] {
            let c = find(&r, &format!("monitoring:claude:{id}"));
            assert_eq!(c.status, Status::Problem);
            assert_eq!(c.fix, Some(Fix::ConnectMonitoring { provider: Provider::Claude, id: id.into() }));
            assert!(c.title.contains(&format!("{id}@example.invalid")));
        }
        assert!(r.checks.iter().all(|c| c.id != "monitoring"), "the all-ok row is replaced");
        assert_eq!((r.status, r.problems), (Status::Problem, 2));
    }

    #[test]
    fn a_rejected_cli_login_and_plaintext_sign_ins_are_reported() {
        let mut case = Case::healthy();
        case.accounts[1].cli_rejected = true;
        case.accounts[1].active = true;
        case.vault_plaintext = 2;
        let r = case.run();
        let c = find(&r, "cli:claude:b");
        assert_eq!((c.status, c.fix.clone()), (Status::Problem, Some(Fix::OpenSettings { page: "accounts" })));
        assert!(c.reason.contains("claude /login") && c.reason.contains("the CLI manages"), "{}", c.reason);
        assert!(r.checks.iter().all(|c| c.id != "cli:claude:a"));
        let v = find(&r, "vault");
        assert_eq!((v.status, v.fix.clone()), (Status::Warning, Some(Fix::RestartApp)));
        assert!(v.reason.starts_with("2 "));
        assert_eq!((r.problems, r.warnings), (1, 1));
    }

    #[test]
    fn an_unmonitored_account_says_whether_it_connects_automatically() {
        let mut case = Case::healthy();
        case.accounts[0].monitor_connected = false;
        let r = case.run();
        let c = find(&r, "monitoring:claude:a");
        assert_eq!(c.status, Status::Warning);
        assert_eq!(c.fix, Some(Fix::ConnectMonitoring { provider: Provider::Claude, id: "a".into() }));
        assert!(c.reason.contains("Connect monitoring automatically is on"), "{}", c.reason);
        case.auto_connect = false;
        let r = case.run();
        let c = find(&r, "monitoring:claude:a");
        assert!(c.reason.contains("Connect monitoring automatically is off"), "{}", c.reason);
    }

    #[test]
    fn a_monitoring_sign_in_expiring_within_three_days_is_a_warning() {
        let mut case = Case::healthy();
        case.monitoring.insert("Claude:a".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() + Duration::hours(30)) });
        case.monitoring.insert("Claude:b".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() + Duration::days(20)) });
        let r = case.run();
        let c = find(&r, "monitoring:claude:a");
        assert_eq!((c.status, c.title.as_str()), (Status::Warning, "a@example.invalid: monitoring sign-in ends soon"));
        assert!(c.reason.contains("within 30 hours") && c.reason.contains("about 30 days") && c.reason.contains("cannot extend"), "{}", c.reason);
        assert!(r.checks.iter().all(|c| c.id != "monitoring:claude:b"), "far-off deadlines are fine");
        // Within RENEW_DAYS but not WARN_DAYS: Connect all takes it along; Health says nothing yet.
        let mut later = Case::healthy();
        later.monitoring.insert("Claude:a".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() + Duration::days(5)) });
        assert_eq!(later.run().status, Status::Ok);
        let mut close = Case::healthy();
        close.monitoring.insert("Claude:a".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() + Duration::minutes(30)) });
        let reason = find(&close.run(), "monitoring:claude:a").reason.clone();
        assert!(reason.contains("within 1 hour;"), "one hour, not hours: {reason}");
        // A passed deadline is only a renewal hint: renewing at the next refresh clears it.
        let mut passed = Case::healthy();
        passed.monitoring.insert("Claude:a".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() - Duration::hours(1)) });
        assert_eq!(passed.run().status, Status::Ok);
    }

    #[test]
    fn an_ended_monitoring_sign_in_says_why_and_a_refused_one_is_still_a_fault() {
        let mut case = Case::healthy();
        case.accounts.push(account("c", true, Some(1)));
        case.monitoring.insert("Claude:a".into(), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::days(2)) });
        case.monitoring.insert("Claude:b".into(), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() + Duration::days(9)) });
        case.monitoring.insert("Claude:c".into(), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() + Duration::days(1)) });
        let r = case.run();
        let a = find(&r, "monitoring:claude:a");
        assert_eq!((a.status, a.title.as_str()), (Status::Problem, "a@example.invalid: monitoring sign-in ended"));
        assert!(a.reason.contains("about 30 days") && a.reason.contains("cannot extend") && a.reason.contains("ended 2 days ago"), "{}", a.reason);
        assert_eq!(a.fix, Some(Fix::ConnectMonitoring { provider: Provider::Claude, id: "a".into() }));
        let b = find(&r, "monitoring:claude:b");
        assert_eq!((b.status, b.title.as_str()), (Status::Problem, "b@example.invalid: monitoring sign-in needs reconnecting"));
        assert!(b.reason.contains("can no longer be renewed, well before the date it was due to end"), "{}", b.reason);
        // The provider may end a sign-in a little before the date it gave: still its limit.
        let c = find(&r, "monitoring:claude:c");
        assert_eq!(c.title, "c@example.invalid: monitoring sign-in ended");
        assert!(c.reason.contains("ended a little early, which the provider may do"), "{}", c.reason);
        // An account not in use carries its monitoring error as its error.
        let mut case = Case::healthy();
        case.accounts[0].error = Some("monitor_uncertain".into());
        assert_eq!(find(&case.run(), "monitoring:claude:a").title, "a@example.invalid: monitoring sign-in needs reconnecting");
        // Reconnected since that error: the saved sign-in says so before the next refresh does.
        case.monitoring.insert("Claude:a".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() + Duration::days(30)) });
        assert_eq!(case.run().status, Status::Ok, "a stale error must not flag a fresh sign-in");
    }

    #[test]
    fn two_or_more_sign_ins_to_make_get_one_row_that_makes_them_all() {
        let mut case = Case::healthy();
        case.accounts.push(account("c", true, Some(1)));
        case.accounts.push(account("d", false, Some(1)));
        case.monitoring.insert("Claude:a".into(), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::hours(3)) });
        case.monitoring.insert("Claude:b".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() + Duration::hours(30)) });
        // Ending in 5 days: no row of its own yet, but signed in along with the others.
        case.monitoring.insert("Claude:c".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() + Duration::days(5)) });
        let r = case.run();
        let first = r.checks.iter().position(|c| c.id.starts_with("monitoring")).unwrap();
        let s = &r.checks[first];
        assert_eq!(s.id, RENEWAL_SUMMARY, "the summary leads the monitoring rows");
        assert_eq!((s.status, s.fix.clone(), s.count), (Status::Problem, Some(Fix::ConnectMonitoringAll), Some(4)));
        assert_eq!(s.title, "Monitoring sign-ins: 1 ended, 1 ends soon, 1 not connected");
        assert!(s.reason.contains("No setting makes it permanent") && s.reason.contains("the 4 accounts") && s.reason.contains("within 7 days"), "{}", s.reason);
        assert!(r.checks.iter().all(|c| c.id != "monitoring:claude:c"));
        assert_eq!((r.problems, r.warnings), (1, 2), "the summary repeats rows already counted");
        // One alone needs no summary; refusals alone say nothing of a limit.
        let mut one = Case::healthy();
        one.monitoring.insert("Claude:a".into(), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::hours(3)) });
        assert!(one.run().checks.iter().all(|c| c.id != RENEWAL_SUMMARY));
        let mut refused = Case::healthy();
        for id in ["a", "b"] { refused.monitoring.insert(format!("Claude:{id}"), MonitorState { reconnect: true, uncertain: false, ends_at: None }); }
        let r = refused.run();
        let s = find(&r, RENEWAL_SUMMARY);
        assert_eq!(s.title, "Monitoring sign-ins: 2 need reconnecting");
        assert!(!s.reason.contains("30 days"), "{}", s.reason);
    }

    #[test]
    fn a_saved_cli_sign_in_that_ended_or_ends_soon_says_so_with_sign_in_all_as_its_fix() {
        let mut case = Case::healthy();
        case.accounts[0].cli_ends_at = Some(now() - Duration::days(2));
        case.accounts[1].cli_ends_at = Some(now() + Duration::hours(30));
        let r = case.run();
        // Accounts not in use: nothing fails until the CLI is switched to one, so a warning.
        let a = find(&r, "cli_signin:claude:a");
        assert_eq!((a.status, a.title.as_str(), a.fix.clone()), (Status::Warning, "a@example.invalid: CLI sign-in ended", Some(Fix::SignInAll)));
        assert!(a.reason.contains("about 30 days") && a.reason.contains("ended 2 days ago") && a.reason.contains("would stop the CLI")
            && a.reason.contains("switches nothing") && a.reason.contains("automatic switching passes it over"), "{}", a.reason);
        let b = find(&r, "cli_signin:claude:b");
        assert_eq!((b.status, b.title.as_str()), (Status::Warning, "b@example.invalid: CLI sign-in ends soon"));
        assert!(b.reason.contains("ends within 30 hours"), "{}", b.reason);
        let s = find(&r, CLI_RENEWAL_SUMMARY);
        assert_eq!((s.status, s.title.as_str(), s.fix.clone(), s.count), (Status::Warning, "CLI sign-ins: 1 ended, 1 ends soon", Some(Fix::SignInAll), Some(2)));
        assert!(s.reason.contains("No setting makes it permanent") && s.reason.contains("the 2 accounts not in use"), "{}", s.reason);
        assert_eq!((r.status, r.problems, r.warnings), (Status::Warning, 0, 2), "the summary repeats rows already counted");
        assert_eq!(serde_json::to_value(&a.fix).unwrap(), serde_json::json!({ "action": "sign_in_all" }));

        // The account in use: the CLI manages its sign-in, so no fix; but once it ended the CLI
        // stops, which an agent running it never hears about from the CLI itself.
        let mut in_use = Case::healthy();
        in_use.accounts[0].active = true;
        in_use.accounts[0].cli_ends_at = Some(now() - Duration::days(2));
        let r = in_use.run();
        let c = find(&r, "cli_signin:claude:a");
        assert_eq!((c.status, c.fix.clone()), (Status::Problem, None));
        assert!(c.reason.contains("in use now, ended 2 days ago") && c.reason.contains("claude /login"), "{}", c.reason);
        assert!(r.checks.iter().all(|c| c.id != CLI_RENEWAL_SUMMARY));
        in_use.accounts[0].cli_ends_at = Some(now() + Duration::hours(20));
        assert_eq!(find(&in_use.run(), "cli_signin:claude:a").status, Status::Warning);
        in_use.accounts[0].cli_ends_at = Some(now() + Duration::days(5));
        assert_eq!(in_use.run().status, Status::Ok);
        // Ending within 7 days but not 3: Sign in all accounts takes it along, Health says nothing yet.
        let mut later = Case::healthy();
        later.accounts[0].cli_ends_at = Some(now() + Duration::days(5));
        later.accounts[1].cli_ends_at = Some(now() + Duration::days(25));
        assert_eq!(later.run().status, Status::Ok);
        // One other that ended: a row of its own, no summary.
        later.accounts.push(account("c", true, Some(1)));
        later.accounts[2].cli_ends_at = Some(now() - Duration::hours(1));
        let r = later.run();
        assert!(r.checks.iter().all(|c| c.id != CLI_RENEWAL_SUMMARY), "one row alone needs no summary");
        assert_eq!(find(&r, "cli_signin:claude:c").status, Status::Warning);
    }

    #[test]
    fn with_other_computers_the_renewal_summaries_sign_them_in_from_here_too() {
        let mut case = Case::healthy();
        case.accounts[0].cli_ends_at = Some(now() - Duration::days(2));
        case.accounts[1].cli_ends_at = Some(now() + Duration::hours(30));
        for id in ["a", "b"] {
            case.monitoring.insert(format!("Claude:{id}"), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::hours(3)) });
        }
        case.other_computers = 2;
        let r = case.run();
        for id in [CLI_RENEWAL_SUMMARY, RENEWAL_SUMMARY] {
            let s = find(&r, id);
            assert_eq!(s.fix, Some(Fix::SignInEverywhere), "{id}");
            assert!(s.reason.ends_with(" Sign in from here adds your 2 other computers' sign-ins, their pages opening here, after you confirm."), "{}", s.reason);
        }
        assert_eq!(find(&r, "cli_signin:claude:a").fix, Some(Fix::SignInAll), "an account's own row stays this computer's");
        assert_eq!(serde_json::to_value(Fix::SignInEverywhere).unwrap(), serde_json::json!({ "action": "sign_in_everywhere" }));
        case.other_computers = 1;
        assert!(find(&case.run(), CLI_RENEWAL_SUMMARY).reason.contains("your other computer's sign-ins"));
        // Monitoring alone, with another computer: from here as well.
        let mut alone = Case::healthy();
        for id in ["a", "b"] {
            alone.monitoring.insert(format!("Claude:{id}"), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::hours(3)) });
        }
        alone.other_computers = 1;
        assert_eq!(find(&alone.run(), RENEWAL_SUMMARY).fix, Some(Fix::SignInEverywhere));
    }

    #[test]
    fn the_renewal_summaries_reach_agents_whole() {
        // get_health's strings reach agents cut at mcp::MAX_TEXT. The longest summaries: Codex's
        // longer limit sentence, monitoring and CLI sign-ins due together, other computers; with
        // room for three-digit counts.
        let mut case = Case::healthy();
        for a in &mut case.accounts {
            a.provider = Provider::Codex;
            a.cli_ends_at = Some(now() - Duration::days(2));
        }
        for id in ["a", "b"] {
            case.monitoring.insert(format!("Codex:{id}"), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::hours(3)) });
        }
        case.other_computers = 99;
        let r = case.run();
        for id in [CLI_RENEWAL_SUMMARY, RENEWAL_SUMMARY] {
            let s = find(&r, id);
            assert_eq!(s.fix, Some(Fix::SignInEverywhere), "{id}");
            assert!(s.reason.starts_with(sign_in_limit(Provider::Codex)), "{id}: {}", s.reason);
            let n = s.reason.chars().count();
            assert!(n + 4 <= crate::mcp::MAX_TEXT, "{id}: {n} characters");
        }
    }

    /// Every title and reason get_health gives reaches an agent whole (`mcp::MAX_TEXT`), at the
    /// longest what it names can make it: Codex's longer limit sentence; sign-ins that ended a
    /// little early, were refused or were never connected, read by another computer with
    /// a 63-character name (the most a roster or presence record keeps); a thousand computers or WSL
    /// distros with long names; 254-character emails; a 1024-character sharing folder or a
    /// 64-character rclone remote, with every outcome and every pair of the network's proxy's reasons.
    #[test]
    fn every_reason_reaches_an_agent_whole() {
        use crate::sources::{DistroStatus, WslPlan, WslScan, WslStatus};
        let whole = |r: &Report| {
            for c in &r.checks {
                for text in [&c.title, &c.reason] {
                    assert!(text.chars().count() <= crate::mcp::MAX_TEXT, "{}: {} characters: {text}", c.id, text.chars().count());
                }
            }
        };
        let email = format!("{}@example.invalid", "e".repeat(254 - "@example.invalid".len()));
        // Short names fill a list's whole room; long ones are clipped.
        for (provider, len) in [(Provider::Claude, 8), (Provider::Claude, 300), (Provider::Codex, 8), (Provider::Codex, 300)] {
            let names: Vec<String> = (0..1000).map(|i| format!("{i:03}{}", "n".repeat(len))).collect();
            let mut case = Case::healthy();
            case.accounts = ["a", "b", "c", "d"].into_iter().map(|id| account(id, true, Some(1))).collect();
            for a in &mut case.accounts {
                a.provider = provider;
                a.email = Some(email.clone());
                a.covered_by = Some("M".repeat(63));
                a.overlap = names.clone();
                a.cli_ends_at = Some(now() - Duration::days(2));
                // A conflict on a and c, a planned share on b and d: both rows' wording.
                a.room_to_share = matches!(a.id.as_str(), "b" | "d");
            }
            // a ended a little early, b was refused well before its end, c was never connected,
            // d is in use, its CLI sign-in ended and the desktop app is on another account.
            case.monitoring.insert(format!("{}:a", provider.name()), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() + Duration::days(1)) });
            case.monitoring.insert(format!("{}:b", provider.name()), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() + Duration::days(9)) });
            case.accounts[2].monitor_connected = false;
            case.accounts[3].active = true;
            case.accounts[3].seats = Some(crate::model::SeatDrift { app: "Claude Desktop".into(), cli: Some(email.clone()), desktop: Some(email.clone()) });
            case.other_computers = 999;
            case.wsl = WslStatus { mode: WslScan::Auto, checked_at: Some(now()), installed: true, timed_out: true, backing_off: false,
                distros: names.iter().map(|n| DistroStatus { name: n.clone(), running: true, plan: WslPlan::Read, has_logins: false, known: false, timed_out: true }).collect() };
            let r = case.run();
            let a = find(&r, &format!("monitoring:{}:a", provider.key()));
            assert!(a.reason.contains("ended a little early") && a.reason.ends_with("needs no account use."), "{}", a.reason);
            let (overlap, drift) = (format!("overlap:{}:a", provider.key()), format!("desktop_drift:{}", provider.key()));
            let shared = format!("overlap:{}:b", provider.key());
            for id in ["wsl", RENEWAL_SUMMARY, CLI_RENEWAL_SUMMARY, overlap.as_str(), shared.as_str(), drift.as_str()] {
                find(&r, id);
            }
            assert_eq!((find(&r, &overlap).status, find(&r, &shared).status), (Status::Warning, Status::Ok));
            whole(&r);
            // WSL answering: what it read and what it left stopped, each list in its room.
            case.wsl.timed_out = false;
            for (i, d) in case.wsl.distros.iter_mut().enumerate() {
                d.timed_out = false;
                d.plan = if i % 2 == 0 { WslPlan::Read } else { WslPlan::Skip };
            }
            let r = case.run();
            assert!(find(&r, "wsl").reason.ends_with("looks once)."), "{}", find(&r, "wsl").reason);
            whole(&r);
        }
        // Sharing: a long folder or remote, every outcome, every pair of the proxy's reasons.
        let codes: Vec<Option<&'static str>> = std::iter::once(None).chain(crate::net::PROXY_CODES.iter().copied().map(Some)).collect();
        let mut case = Case::healthy();
        for (folder, remote) in [(Some(format!("C:\\{}", "f".repeat(1021))), None), (None, Some("r".repeat(64)))] {
            case.sharing = Sharing { folder: folder.clone(), remote: remote.clone(), rclone_installed: true, last: None };
            whole(&case.run());
            for (wrote, read) in [(true, true), (false, true), (true, false), (false, false)] {
                for &write_error in &codes {
                    for &read_error in &codes {
                        case.sharing.last = Some(SharingResult { at: now(), wrote, read, write_error, read_error });
                        let r = case.run();
                        whole(&r);
                        let reason = &find(&r, "sharing").reason;
                        // The place is clipped, never what is said after it.
                        if !wrote || !read {
                            assert!(reason.ends_with('.'), "{reason}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn monitoring_and_cli_sign_ins_due_together_are_made_in_one_sitting() {
        let mut case = Case::healthy();
        for id in ["a", "b"] {
            case.monitoring.insert(format!("Claude:{id}"), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::hours(3)) });
        }
        assert_eq!(find(&case.run(), RENEWAL_SUMMARY).fix, Some(Fix::ConnectMonitoringAll), "monitoring alone: Connect all");
        case.accounts[0].cli_ends_at = Some(now() - Duration::hours(3));
        let s = find(&case.run(), RENEWAL_SUMMARY).clone();
        assert_eq!(s.fix, Some(Fix::SignInAll), "Sign in all accounts makes both, an email's in one browser window");
        assert!(s.reason.contains("the 1 saved CLI sign-in due"), "{}", s.reason);
        // A rejected saved Claude login of an account not in use is one Sign in all accounts makes
        // again too, and, like one that ended, only a warning.
        let mut rejected = Case::healthy();
        rejected.accounts[1].cli_rejected = true;
        let c = find(&rejected.run(), "cli:claude:b").clone();
        assert_eq!((c.status, c.fix.clone()), (Status::Warning, Some(Fix::SignInAll)));
        assert!(c.reason.contains("Sign in all accounts signs it in again"), "{}", c.reason);
        // A Codex one may only have aged out: the old advice, no sign-in queued.
        rejected.accounts[1].provider = Provider::Codex;
        let c = find(&rejected.run(), "cli:codex:b").clone();
        assert_eq!((c.status, c.fix.clone()), (Status::Warning, Some(Fix::OpenSettings { page: "accounts" })));
        assert!(c.reason.contains("codex login"), "{}", c.reason);
    }

    #[test]
    fn a_token_only_account_whose_cli_sign_in_ended_is_said_but_not_offered_sign_in_all() {
        let mut case = Case::healthy();
        for (i, id) in ["token:abc", "token:def"].into_iter().enumerate() {
            case.accounts[i].id = id.into();
            case.accounts[i].cli_ends_at = Some(now() - Duration::days(1));
        }
        let r = case.run();
        let c = find(&r, "cli_signin:claude:token:abc");
        assert_eq!((c.status, c.fix.clone()), (Status::Warning, Some(Fix::OpenSettings { page: "accounts" })));
        assert!(c.reason.contains("cannot sign it in again"), "{}", c.reason);
        assert!(r.checks.iter().all(|c| c.id != CLI_RENEWAL_SUMMARY), "Sign in all accounts would renew none of them");
    }

    #[test]
    fn an_ended_sign_in_s_old_reading_is_not_reported_twice() {
        let mut case = Case::healthy();
        case.accounts[0].fetched_at = Some(now() - Duration::hours(10));
        case.monitoring.insert("Claude:a".into(), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::hours(10)) });
        let r = case.run();
        assert!(r.checks.iter().any(|c| c.id == "monitoring:claude:a"));
        assert!(r.checks.iter().all(|c| c.id != "freshness:claude:a"), "a refresh cannot bring it back: the monitoring row says why");
        // In use, it is read with the CLI's own sign-in, so an old reading there is news.
        case.accounts[0].active = true;
        assert!(case.run().checks.iter().any(|c| c.id == "freshness:claude:a"));
    }

    #[test]
    fn a_sign_in_another_computer_covers_is_a_warning_that_names_it() {
        let mut case = Case::healthy();
        case.accounts.push(account("c", false, None));
        for id in ["a", "b"] {
            case.monitoring.insert(format!("Claude:{id}"), MonitorState { reconnect: true, uncertain: false, ends_at: Some(now() - Duration::days(2)) });
        }
        // Another computer read a and c recently (a is in use here, read through the CLI, or not:
        // either way covered); b nobody covers.
        case.accounts[0].covered_by = Some("DESK-A".into());
        case.accounts[2].covered_by = Some("DESK-A".into());
        let r = case.run();
        let a = find(&r, "monitoring:claude:a");
        assert_eq!((a.status, a.title.as_str()), (Status::Warning, "a@example.invalid: monitoring sign-in ended"));
        assert!(a.reason.contains("ended 2 days ago") && a.reason.contains("DESK-A still reads this account's usage, which shows here")
            && a.reason.contains("Automatic switching and alerts here go by this computer's own readings"), "{}", a.reason);
        assert_eq!(a.fix, Some(Fix::ConnectMonitoring { provider: Provider::Claude, id: "a".into() }), "reconnecting stays offered");
        assert_eq!(find(&r, "monitoring:claude:b").status, Status::Problem);
        let c = find(&r, "monitoring:claude:c");
        assert_eq!(c.status, Status::Warning);
        assert!(c.reason.contains("DESK-A still reads this account's usage"), "{}", c.reason);
        let s = find(&r, RENEWAL_SUMMARY);
        assert_eq!((s.status, s.title.as_str()), (Status::Problem, "Monitoring sign-ins: 2 ended, 1 not connected (2 covered by your other computers' readings)"));
        assert_eq!((r.status, r.problems, r.warnings), (Status::Problem, 1, 2));
        // All covered: nothing is a problem, the summary included, and nothing reads as one.
        case.accounts[1].covered_by = Some("LAPTOP-B".into());
        let r = case.run();
        assert_eq!((r.status, r.problems, r.warnings), (Status::Warning, 0, 3));
        assert_eq!(find(&r, RENEWAL_SUMMARY).title, "Monitoring sign-ins: 2 ended, 1 not connected (3 covered by your other computers' readings)");
    }

    #[test]
    fn a_refused_sign_in_nobody_covers_is_a_problem_and_a_covered_one_ending_soon_counts_as_covered() {
        let mut case = Case::healthy();
        case.monitoring.insert("Claude:a".into(), MonitorState { reconnect: true, uncertain: false, ends_at: None });
        case.monitoring.insert("Claude:b".into(), MonitorState { reconnect: false, uncertain: false, ends_at: Some(now() + Duration::hours(30)) });
        case.accounts[1].covered_by = Some("DESK-A".into());
        let r = case.run();
        assert_eq!(find(&r, "monitoring:claude:a").status, Status::Problem);
        assert_eq!(find(&r, "monitoring:claude:b").status, Status::Warning);
        let s = find(&r, RENEWAL_SUMMARY);
        assert_eq!((s.status, s.title.as_str()), (Status::Problem, "Monitoring sign-ins: 1 needs reconnecting, 1 ends soon (1 covered by your other computers' readings)"));
        // Covered too: no problem left.
        case.accounts[0].covered_by = Some("DESK-A".into());
        assert_eq!(find(&case.run(), RENEWAL_SUMMARY).status, Status::Warning);
    }

    #[test]
    fn transient_monitoring_failures_warn_and_retry() {
        let mut case = Case::healthy();
        case.accounts[0].monitor_error = Some("monitor_network".into());
        case.accounts[1].monitor_error = Some("weird\nprovider text that is long".into());
        let r = case.run();
        let a = find(&r, "monitoring:claude:a");
        assert_eq!((a.status, a.fix.clone()), (Status::Warning, Some(Fix::Refresh)));
        let b = find(&r, "monitoring:claude:b");
        assert!(!b.reason.contains('\n') && !b.reason.contains("provider text that is long"), "raw errors are not echoed: {}", b.reason);
        case.accounts[0].monitor_error = Some("monitor_storage".into());
        assert_eq!(find(&case.run(), "monitoring:claude:a").status, Status::Problem);
    }

    #[test]
    fn accounts_without_monitoring_are_warnings_to_connect() {
        let mut case = Case::healthy();
        case.accounts.push(account("c", false, Some(600)));
        let r = case.run();
        let c = find(&r, "monitoring:claude:c");
        assert_eq!((c.status, c.fix.clone()), (Status::Warning, Some(Fix::ConnectMonitoring { provider: Provider::Claude, id: "c".into() })));
        assert!(r.checks.iter().all(|c| c.id != "freshness:claude:c"), "its old reading is explained by the monitoring row");
        assert_eq!((r.status, r.warnings), (Status::Warning, 1));
    }

    #[test]
    fn readings_older_than_two_intervals_are_stale() {
        let mut case = Case::healthy();
        case.accounts[0].fetched_at = Some(now() - Duration::minutes(11) - Duration::seconds(30));
        case.accounts[1].stale = true;
        let mut active = account("in-use", false, Some(30));
        active.active = true;
        case.accounts.push(active);
        let r = case.run();
        assert_eq!(find(&r, "freshness:claude:a").status, Status::Warning);
        assert!(find(&r, "freshness:claude:a").reason.contains("11 minutes ago"));
        assert_eq!(find(&r, "freshness:claude:b").fix, Some(Fix::Refresh));
        assert_eq!(find(&r, "freshness:claude:in-use").status, Status::Warning, "the account in use is read through its CLI login");
        // Ten minutes and a half is within two five-minute intervals plus slack.
        case.accounts = vec![account("a", true, Some(10))];
        assert_eq!(find(&case.run(), "freshness").status, Status::Ok);
    }

    #[test]
    fn last_successful_refresh_age() {
        let mut case = Case::healthy();
        case.last_success = None;
        let r = case.run();
        assert_eq!(find(&r, "refresh").status, Status::Warning);
        assert!(r.checks.iter().all(|c| !c.id.starts_with("freshness:")), "no staleness before the first refresh");
        case.last_success = Some(now() - Duration::minutes(45));
        let r = case.run();
        let c = find(&r, "refresh");
        assert_eq!((c.status, c.fix.clone()), (Status::Problem, Some(Fix::Refresh)));
        assert!(c.reason.contains("45 minutes ago"), "{}", c.reason);
    }

    #[test]
    fn the_agent_door() {
        let mut case = Case::healthy();
        case.door = Door::Retrying;
        let c = find(&case.run(), "door").clone();
        assert_eq!((c.status, c.fix), (Status::Problem, Some(Fix::RestartApp)));
        case.door = Door::Starting;
        assert_eq!(find(&case.run(), "door").status, Status::Warning);
        case.door = Door::Unsupported;
        assert!(case.run().checks.iter().all(|c| c.id != "door"));
    }

    #[test]
    fn the_roster_says_how_many_accounts_wait_and_why_it_cannot_share() {
        let mut case = Case::healthy();
        assert_eq!(find(&case.run(), "roster").reason.split(':').next(), Some("Off"), "off is reported too");
        assert!(case.run().checks.iter().all(|c| c.id != "roster_waiting"));
        case.roster = Roster { enabled: true, ..Default::default() };
        assert!(find(&case.run(), "roster").reason.contains("sharing is off"));
        case.sharing.folder = Some("D:\\Sync".into());
        assert!(find(&case.run(), "roster").reason.contains("passphrase"));
        case.roster.passphrase_saved = true;
        case.roster.wrote = Some(true);
        case.roster.read = Some(true);
        case.roster.unreadable = 1;
        assert_eq!(find(&case.run(), "roster").status, Status::Warning);
        case.roster.unreadable = 0;
        // Its own roster found changed and written again: once or twice is a restore put right
        // quietly; from three times something keeps writing it.
        case.roster.own_restores = crate::roster::OWN_REWRITTEN_WARN - 1;
        assert_eq!(find(&case.run(), "roster").status, Status::Ok);
        case.roster.own_restores = crate::roster::OWN_REWRITTEN_WARN;
        let rewritten = find(&case.run(), "roster").clone();
        assert_eq!(rewritten.status, Status::Warning);
        assert!(rewritten.reason.starts_with("This computer's roster was found changed or gone, and written again, 3 times in a row"), "{}", rewritten.reason);
        case.roster.own_restores = 0;
        case.roster.waiting = 13;
        let r = case.run();
        assert_eq!(find(&r, "roster").status, Status::Ok);
        let waiting = find(&r, "roster_waiting");
        assert_eq!((waiting.status, waiting.count), (Status::Warning, Some(13)));
        assert_eq!(waiting.title, "13 accounts from your other machines are waiting to be signed in");
        assert_eq!(waiting.fix, Some(Fix::OpenSettings { page: "accounts" }));
        case.roster.read = Some(false);
        assert_eq!(find(&case.run(), "roster").status, Status::Problem);
    }

    #[test]
    fn sharing_that_cannot_write_or_read_points_to_data_settings() {
        let mut case = Case::healthy();
        case.sharing = Sharing { folder: Some("D:\\Sync".into()), remote: None, rclone_installed: false, last: None };
        assert_eq!(find(&case.run(), "sharing").status, Status::Ok, "not yet tried");
        assert!(case.run().checks.iter().all(|c| c.id != "rclone"), "rclone only matters for a remote");
        for (wrote, read, status) in [(true, true, Status::Ok), (false, true, Status::Problem), (true, false, Status::Problem), (false, false, Status::Problem)] {
            case.sharing.last = Some(SharingResult { at: now() - Duration::minutes(2), wrote, read, write_error: None, read_error: None });
            let r = case.run();
            let c = find(&r, "sharing");
            assert_eq!(c.status, status, "wrote {wrote} read {read}");
            if status == Status::Problem {
                assert_eq!(c.fix, Some(Fix::OpenSettings { page: "data" }));
            }
        }
    }

    #[test]
    fn a_sharing_result_keeps_only_the_proxys_code() {
        use crate::proxy::{ProxyRefused, Refusal};
        let refused = || -> std::io::Result<()> { Err(std::io::Error::other(ProxyRefused(Refusal::Script))) };
        let failed = || -> std::io::Result<()> { Err(std::io::Error::other("rclone rcat failed (Some(1))")) };
        let r = SharingResult::of(now(), &refused(), &Ok::<_, std::io::Error>(vec![1]));
        assert_eq!((r.wrote, r.read, r.write_error, r.read_error), (false, true, Some("proxy_script"), None));
        let r = SharingResult::of(now(), &failed(), &refused());
        assert_eq!((r.wrote, r.read, r.write_error, r.read_error), (false, false, None, Some("proxy_script")), "each failure keeps its own reason");
        let r = SharingResult::of(now(), &failed(), &failed());
        assert_eq!((r.write_error, r.read_error), (None, None), "any other failure has no code");
        let ok = Ok::<_, std::io::Error>(());
        assert_eq!(SharingResult::of(now(), &ok, &ok), SharingResult { at: now(), wrote: true, read: true, write_error: None, read_error: None });
        assert_eq!(SharingResult::failed(now()), SharingResult { at: now(), wrote: false, read: false, write_error: None, read_error: None });
    }

    #[test]
    fn sharing_the_roster_and_monitoring_say_when_the_networks_proxy_stopped_them() {
        // The English text Settings, the flyout and the widget show for the code (i18n.js).
        let script = crate::i18n::t("en", "proxy_script", &[]);
        let unsupported = crate::i18n::t("en", "proxy_unsupported", &[]);
        assert!(script.contains("proxy") && unsupported.contains("proxy"), "{script} / {unsupported}");
        let mut case = Case::healthy();
        case.sharing = Sharing { folder: None, remote: Some("dropbox".into()), rclone_installed: true, last: None };
        let script_where = |wrote: bool, read: bool| (if wrote { None } else { Some("proxy_script") }, if read { None } else { Some("proxy_script") });
        for (wrote, read, words) in [
            (false, true, "Could not write to the rclone remote dropbox, so other computers do not see this one."),
            (true, false, "Could not read the rclone remote dropbox, so this computer does not see the others."),
            (false, false, "Could not write to or read the rclone remote dropbox."),
        ] {
            let (write_error, read_error) = script_where(wrote, read);
            case.sharing.last = Some(SharingResult { at: now(), wrote, read, write_error, read_error });
            let c = find(&case.run(), "sharing").clone();
            assert_eq!(c.reason, format!("{words} {script}"));
            assert_eq!((c.status, c.fix), (Status::Problem, Some(Fix::OpenSettings { page: "data" })));
            // Any other failure keeps its own words.
            case.sharing.last = Some(SharingResult { at: now(), wrote, read, write_error: None, read_error: None });
            assert_eq!(find(&case.run(), "sharing").reason, words);
        }
        // Both failed, only one for the proxy (the route changed between them): said of that one.
        case.sharing.last = Some(SharingResult { at: now(), wrote: false, read: false, write_error: None, read_error: Some("proxy_script") });
        assert_eq!(find(&case.run(), "sharing").reason, format!("Could not write to or read the rclone remote dropbox. Reading: {script}"));
        case.sharing.last = Some(SharingResult { at: now(), wrote: false, read: false, write_error: Some("proxy_unsupported"), read_error: Some("proxy_script") });
        assert_eq!(find(&case.run(), "sharing").reason, format!("Could not write to or read the rclone remote dropbox. Writing: {unsupported} Reading: {script}"));

        case.roster = Roster { enabled: true, passphrase_saved: true, wrote: Some(false), read: Some(false),
            write_error: Some("proxy_unsupported"), read_error: Some("proxy_script"), ..Default::default() };
        let c = find(&case.run(), "roster").clone();
        assert_eq!(c.reason, format!("Could not write this computer's roster, so your other computers do not learn its accounts. {unsupported}"));
        assert_eq!(c.status, Status::Problem);
        case.roster.wrote = Some(true);
        assert_eq!(find(&case.run(), "roster").reason, format!("Could not read the rosters, so this computer does not learn the others' accounts. {script}"));
        case.roster.read_error = None;
        assert_eq!(find(&case.run(), "roster").reason, "Could not read the rosters, so this computer does not learn the others' accounts.");

        // Monitoring's renewal, retried as after any connection failure.
        case.accounts[0].monitor_error = Some("proxy_script".into());
        let c = find(&case.run(), "monitoring:claude:a").clone();
        assert_eq!((c.status, c.fix.clone()), (Status::Warning, Some(Fix::Refresh)));
        assert_eq!(c.reason, format!("Monitoring could not renew its sign-in at platform.claude.com. {script} \
            SlopBar tries the renewal again at the first refresh at least 5 minutes after it failed, or when it starts again."));

        // An account the proxy keeps from being read at all (its usage, or monitoring's renewal
        // with no CLI to fall back on): its out-of-date row says why.
        case.accounts[1].fetched_at = Some(now() - Duration::minutes(30));
        case.accounts[1].stale = true;
        case.accounts[1].error = Some("proxy_unsupported".into());
        assert_eq!(find(&case.run(), "freshness:claude:b").reason, format!("Last confirmed 30 minutes ago, more than two refresh intervals ago. {unsupported}"));
        case.accounts[1].error = Some("monitor_network".into());
        assert_eq!(find(&case.run(), "freshness:claude:b").reason, "Last confirmed 30 minutes ago, more than two refresh intervals ago.");

        // Every account stopped by the proxy from the start: before a first read nothing else has
        // anything to say, so the last refresh says why.
        let environment = crate::i18n::t("en", "proxy_environment", &[]);
        let mut start = Case::healthy();
        start.last_success = None;
        for a in &mut start.accounts {
            a.error = Some("proxy_environment".into());
        }
        assert_eq!(find(&start.run(), "refresh").reason, format!("No refresh has read any account yet. {environment}"));
        start.last_success = Some(now() - Duration::minutes(45));
        assert!(find(&start.run(), "refresh").reason.ends_with(&format!("refreshes run every 5 minutes. {environment}")));
        // Not all of them: the accounts' own rows say which.
        start.accounts[0].error = None;
        assert_eq!(find(&start.run(), "refresh").reason, "The last refresh that read usage was 45 minutes ago; refreshes run every 5 minutes.");
    }

    #[test]
    fn a_chosen_remote_without_rclone_is_a_problem() {
        let mut case = Case::healthy();
        case.sharing = Sharing { folder: Some("D:\\Sync".into()), remote: Some("gdrive".into()), rclone_installed: false, last: None };
        let r = case.run();
        assert_eq!(find(&r, "rclone").status, Status::Problem);
        assert_eq!(find(&r, "rclone").fix, Some(Fix::OpenSettings { page: "data" }));
        assert_eq!(find(&r, "sharing").status, Status::Problem);
        case.sharing.rclone_installed = true;
        assert_eq!(find(&case.run(), "rclone").status, Status::Ok);
    }

    #[test]
    fn desktop_app_on_another_account_is_a_warning() {
        let mut case = Case::healthy();
        assert!(case.run().checks.iter().all(|c| !c.id.starts_with("desktop_drift:")));
        case.accounts[0].active = true;
        case.accounts[0].seats = Some(crate::model::SeatDrift { app: "Claude Desktop".into(), cli: Some("a".into()), desktop: None });
        let r = case.run();
        let c = r.checks.iter().find(|c| c.id.starts_with("desktop_drift:")).unwrap();
        assert_eq!(c.status, Status::Warning);
        assert!(c.reason.starts_with("CLI: a · Desktop: an account SlopBar does not have"), "{}", c.reason);
    }

    #[test]
    fn an_account_on_several_computers_is_a_warning() {
        let mut case = Case::healthy();
        assert!(case.run().checks.iter().all(|c| !c.id.starts_with("overlap:")));
        case.accounts[0].overlap = vec!["DESK-1".into(), "LAPTOP-2".into()];
        let r = case.run();
        let c = find(&r, "overlap:claude:a");
        assert_eq!((c.status, r.warnings), (Status::Warning, 1));
        assert!(c.title.ends_with("in use on 2 computers at once"), "{}", c.title);
        assert!(c.reason.starts_with("In use on DESK-1, LAPTOP-2 at the same time: its limits drain about 2x as fast."), "{}", c.reason);
        // Why it is no planned share, and nothing SlopBar has no ground for: no sign-out is promised.
        assert!(c.reason.contains("SlopBar has not seen it stay under its weekly limits in recent weeks"), "{}", c.reason);
        assert!(!c.reason.contains("sign"), "{}", c.reason);
        assert_eq!(c.fix, Some(Fix::OpenSettings { page: "accounts" }));
    }

    #[test]
    fn an_account_shared_within_its_limits_is_a_planned_share() {
        let mut case = Case::healthy();
        case.accounts[0].overlap = vec!["DESK-1".into(), "LAPTOP-2".into(), "TOWER-3".into()];
        case.accounts[0].room_to_share = true;
        let r = case.run();
        let c = find(&r, "overlap:claude:a");
        assert_eq!((c.status, r.status, r.warnings), (Status::Ok, Status::Ok, 0), "no warning: {c:?}");
        assert_eq!(c.title, "a@example.invalid: shared by 3 computers");
        assert_eq!(c.reason, "In use on DESK-1, LAPTOP-2, TOWER-3 at the same time. In its busiest recent weeks it stayed \
            under its weekly limits, so these computers' use adds up within them; the 5-hour limit is shared too.");
        assert_eq!(c.fix, None);
        // Room alone says nothing: only an account on several computers gets a row.
        case.accounts[0].overlap.clear();
        assert!(case.run().checks.iter().all(|c| !c.id.starts_with("overlap:")));
    }

    #[test]
    fn the_diagnostics_journal() {
        let mut case = Case::healthy();
        case.journal = Journal { started: true, write_attempts: 3, storage_errors: 3, last_write_ok: Some(false), history_unreadable: false };
        let c = find(&case.run(), "journal").clone();
        assert_eq!(c.status, Status::Problem);
        assert!(c.reason.contains("3 of 3"));
        case.journal.last_write_ok = Some(true);
        assert_eq!(find(&case.run(), "journal").status, Status::Ok, "a later write succeeded");
        case.journal.history_unreadable = true;
        assert_eq!(find(&case.run(), "journal").status, Status::Warning);
        case.journal = Journal { started: true, ..Journal::default() };
        assert_eq!(find(&case.run(), "journal").status, Status::Ok);
    }

    #[test]
    fn notification_registration_and_windows_settings() {
        let mut case = Case::healthy();
        case.notifications.registered = Some(false);
        let c = find(&case.run(), "notifications").clone();
        assert_eq!((c.status, c.fix), (Status::Warning, Some(Fix::RestartApp)));
        case.notifications.app_enabled = Some(false);
        assert!(find(&case.run(), "notifications").reason.contains("for SlopBar"));
        case.notifications.system_enabled = Some(false);
        assert!(find(&case.run(), "notifications").reason.contains("every app"));
        case.notifications.any_wanted = false;
        assert_eq!(find(&case.run(), "notifications").status, Status::Ok, "nothing to show, nothing wrong");
    }

    #[test]
    fn wsl_shows_only_when_relevant_and_warns_when_it_hangs() {
        use crate::sources::{DistroStatus, WslPlan, WslScan, WslStatus};
        let mut c = Case::healthy();
        // No WSL on this PC, or not looked at yet: no row.
        assert!(c.run().checks.iter().all(|k| k.id != "wsl"));
        c.wsl = WslStatus { checked_at: Some(now()), installed: false, ..Default::default() };
        assert!(c.run().checks.iter().all(|k| k.id != "wsl"));
        let distro = |name: &str, running, plan| DistroStatus { name: name.into(), running, plan, has_logins: false, known: false, timed_out: false };
        c.wsl = WslStatus { mode: WslScan::Auto, checked_at: Some(now()), installed: true, timed_out: false, backing_off: false,
            distros: vec![distro("Debian", true, WslPlan::Read), distro("Ubuntu-22.04", false, WslPlan::Skip)] };
        let r = c.run();
        let k = find(&r, "wsl");
        assert_eq!(k.status, Status::Ok);
        assert!(k.reason.contains("Read: Debian.") && k.reason.contains("Left stopped: Ubuntu-22.04"), "{}", k.reason);
        // A hung WSL is a warning with a way out, not a stalled refresh.
        c.wsl.timed_out = true;
        let r = c.run();
        let k = find(&r, "wsl");
        assert_eq!((k.status, k.fix.clone()), (Status::Warning, Some(Fix::OpenSettings { page: "data" })));
        assert!(k.reason.contains("wsl --shutdown"));
        // Waiting out one slow listing (the service waking up) keeps the last good picture.
        c.wsl.timed_out = false;
        c.wsl.backing_off = true;
        assert_eq!(find(&c.run(), "wsl").status, Status::Ok);
        c.wsl = WslStatus { mode: WslScan::Off, checked_at: Some(now()), installed: true, ..Default::default() };
        assert!(find(&c.run(), "wsl").reason.contains("Off"));
    }

    #[test]
    fn the_report_serializes_for_agents() {
        let mut case = Case::healthy();
        case.door = Door::Retrying;
        let v = serde_json::to_value(case.run()).unwrap();
        assert_eq!(v["status"], "problem");
        assert_eq!(v["problems"], 1);
        let door = v["checks"].as_array().unwrap().iter().find(|c| c["id"] == "door").unwrap();
        assert_eq!(door["fix"], serde_json::json!({ "action": "restart_app" }));
    }
}
