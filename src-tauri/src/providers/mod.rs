pub mod claude;
pub mod codex;
pub mod evidence;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use crate::monitoring::{self, Monitor};

use crate::accounts::{Active, Provider, Slot};
use crate::model::{AccountUsage, ResetGrant, UsageWindow};
use crate::settings::Settings;
use crate::diagnostics::{self, Code, Event, Origin};

/// Last successful numbers per account, used when its saved token can't be used or the usage
/// API asks us to slow down.
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Cached {
    windows: Vec<UsageWindow>,
    credits: Option<String>,
    resets: Vec<ResetGrant>,
    at: Option<DateTime<Utc>>,
    /// Don't call the usage API for this account before this time (after a 429).
    blocked_until: Option<DateTime<Utc>>,
    error: Option<String>,
    monitor_error: Option<String>,
    /// The provider's own billing record, last read at most a few times a day.
    billing: Option<crate::model::BillingHints>,
    /// Claude's extra usage, from the last successful read.
    extra_usage: Option<crate::model::ExtraUsage>,
    /// When `resets` were last read (a reply that carried them), and when a reading last asked
    /// for them (Claude's reset status; see `ask_resets`).
    resets_read_at: Option<DateTime<Utc>>,
    resets_tried_at: Option<DateTime<Utc>>,
    /// The last status read said Claude lists this account's resets only to other clients.
    resets_withheld: bool,
    /// The last ask failed and the reading was taken again without it.
    resets_refused: bool,
    #[serde(skip)]
    verified: bool,
}

/// An account is never polled more often than this, however often the app refreshes.
const MIN_INTERVAL: i64 = 60;
/// Wait at least this long after a 429, even if the API asks for less.
const MIN_BACKOFF: i64 = 5 * 60;
/// From this much of any window used, a Claude reading asks for the reset status at every poll
/// (`ask_resets`): a reset is about to matter, and a limit reached then finds it current.
const RESETS_NEAR_LIMIT: f64 = 90.0;

/// Whether this poll of an account asks for its reset status, decided once from its last reading
/// so that every try of the poll (monitoring, its renewal, the CLI fallback) asks alike. Claude only:
/// Codex reads its credits with every reading. Hourly; but daily while Claude says it lists the
/// account's resets only to other clients (it keeps saying so) or after an ask failed (the reading
/// was then taken again without it); and, while a window is at least `RESETS_NEAR_LIMIT` used and
/// the last ask was answered, at every poll but at most every 5 minutes. An asking reading that
/// failed stops that for an hour (`merge`). Asking adds no request: it is a parameter of the
/// reading, read again without it only when that request fails (`claude::fetch`).
pub(crate) fn ask_resets(provider: Provider, last: Option<&Cached>, now: DateTime<Utc>) -> bool {
    if provider != Provider::Claude { return false; }
    let Some(tried) = last.and_then(|c| c.resets_tried_at) else { return true };
    let c = last.unwrap();
    let since = now - tried;
    if since < chrono::Duration::zero() { return true; }
    if c.resets_refused || c.resets_withheld { return since >= chrono::Duration::hours(24); }
    if since >= chrono::Duration::hours(1) { return true; }
    let answered = c.resets_read_at.is_some_and(|r| r >= tried);
    answered && since >= chrono::Duration::minutes(5) && c.windows.iter().any(|w| w.used_percent >= RESETS_NEAR_LIMIT)
}

/// Error code for a 429, carrying the server's Retry-After (seconds) when present.
pub fn rate_limited(resp: &reqwest::Response) -> String {
    let secs = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0);
    format!("rate_limited:{secs}")
}

pub type Cache = HashMap<String, Cached>;

pub fn retry_now(cache: &mut Cache, key: &str) {
    if let Some(c) = cache.get_mut(key) { c.blocked_until = None; c.verified = false; }
}

pub fn load_cache() -> Cache {
    crate::settings::app_dir().and_then(|d| std::fs::read(d.join("usage-cache.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_cache(cache: &Cache) -> Result<(), String> {
    let dir = crate::settings::app_dir().ok_or("cache_storage")?;
    let bytes = serde_json::to_vec(cache).map_err(|_| "cache_storage")?;
    crate::private_store::write(&dir.join("usage-cache.json"), &bytes).map_err(|_| "cache_storage".into())
}

pub async fn fetch_all(
    client: &reqwest::Client,
    slots: &[Slot],
    active: &Active,
    cache: &mut Cache,
    settings: &Settings,
    monitor: &Monitor,
) -> Vec<AccountUsage> {
    let enabled = |p: Provider| match p {
        Provider::Claude => settings.providers.claude,
        Provider::Codex => settings.providers.codex,
    };
    let slots: Vec<&Slot> = slots.iter().filter(|s| enabled(s.provider)).collect();
    let now = Utc::now();
    // Skip accounts measured moments ago or backing off after a 429: reuse their numbers.
    let skip = |s: &Slot| {
        cache.get(&monitoring::key(s)).is_some_and(|c| {
            c.blocked_until.is_some_and(|t| t > now)
                || (c.verified && c.error.is_none() && c.at.is_some_and(|t| (now - t).num_seconds() < MIN_INTERVAL))
        })
    };
    let asks: Vec<bool> = slots.iter().map(|s| ask_resets(s.provider, cache.get(&monitoring::key(s)), now)).collect();
    let results = futures::future::join_all(slots.iter().zip(&asks).map(|(s, ask)| {
        let skipped = skip(s);
        let is_active = active.contains_key(&(s.provider, s.id.clone()));
        async move { if skipped { None } else { Some(fetch_one(client, s, monitor, is_active, *ask).await) } }
    }))
    .await;

    let mut out: Vec<AccountUsage> = slots
        .iter()
        .zip(results)
        .zip(asks)
        .map(|((slot, result), asked)| {
            let sources = active.get(&(slot.provider, slot.id.clone())).cloned().unwrap_or_default();
            let in_use = !sources.is_empty();
            let mut acc = AccountUsage {
                id: slot.id.clone(),
                provider: slot.provider,
                email: slot.email.clone(),
                alias: settings.aliases.get(&alias_key(slot.provider, &slot.id)).cloned(),
                plan: slot.plan.clone(),
                org: slot.org.clone(),
                active: !sources.is_empty(),
                sources,
                windows: vec![],
                credits: None,
                resets: vec![],
                error: None,
                monitor_error: None,
                stale: false,
                fetched_at: None,
                monitor_connected: monitor.connected(slot),
                monitor_ends_at: None,
                monitor_renewal: None,
                billing: slot.billing(),
                remote: vec![], overlap: vec![], shared: None, covered_by: None,
                used_outside: false,
                runs_out_at: None, unexpected: None, cli_rejected: false, extra_usage: None, leased: None, desktop: false, seats: None, resets_withheld: false, resets_read_at: None,
                // When the saved CLI sign-in ends, and whether it needs a new one while the
                // account is not in use (the CLI manages the one in use).
                cli_ends_at: slot.ends_at(),
                cli_renewal: crate::accounts::cli_renewal(slot.ends_at(), in_use, now),
            };
            let entry = cache.entry(monitoring::key(slot)).or_default();
            merge(entry, result, asked, Utc::now());
            acc.windows = entry.windows.clone();
            acc.credits = entry.credits.clone();
            // Titles cleaned again as shown, for those a reading by an older build kept as they came.
            acc.resets = unexpired(entry.resets.clone(), Utc::now()).into_iter().map(|mut r| {
                r.title = match slot.provider { Provider::Codex => r.title.as_deref().and_then(codex::title), Provider::Claude => None };
                r
            }).collect();
            acc.resets_withheld = entry.resets_withheld;
            acc.resets_read_at = entry.resets_read_at;
            acc.extra_usage = entry.extra_usage.clone();
            acc.fetched_at = entry.at;
            acc.stale = !entry.verified || entry.error.is_some();
            acc.error = entry.error.clone();
            acc.monitor_error = entry.monitor_error.clone();
            acc.cli_rejected = cli_rejected(slot);
            if let Some(b) = &entry.billing { acc.billing = acc.billing.clone().overlay(b); }
            if let Some(p) = &acc.billing.plan_type { acc.plan = Some(p.clone()); }
            // The account record names the Pro tiers too, for when usage has not said.
            else if let Some(tier) = match acc.billing.plan_code.as_deref() { Some("chatgptprolite") => Some("prolite"), Some("chatgptpromax") => Some("promax"), _ => None } {
                acc.plan = Some(tier.into());
            }
            acc
        })
        .collect();
    // Where each monitoring sign-in stands, read after this refresh's renewals: when it ends, and
    // whether it needs a new sign-in (`monitoring::renewal`).
    let connected: Vec<(Provider, String)> = out.iter().filter(|a| a.monitor_connected).map(|a| (a.provider, a.id.clone())).collect();
    let states = monitor.health(&connected).await;
    let now = Utc::now();
    for a in out.iter_mut().filter(|a| a.monitor_connected) {
        let state = states.get(&format!("{}:{}", a.provider.name(), a.id));
        a.monitor_ends_at = state.and_then(|s| s.ends_at);
        // An account not in use carries its monitoring error as its error.
        let error = a.monitor_error.as_deref().or(a.error.as_deref());
        a.monitor_renewal = monitoring::renewal(state, error, now).kind();
    }
    // A stable order: switching accounts must not reshuffle the list.
    out.sort_by_key(|a| (a.provider != Provider::Claude, a.email.clone(), a.id.clone()));
    out
}

/// Folds one poll's outcome into the account's cached reading (`fetch_all`). A success replaces it,
/// keeping what the reply did not carry: the billing record between its reads, and the last resets
/// read (less those expired since) when the reply had no reset status, so a reading that did not
/// ask never clears them. A failure keeps the last numbers and backs off; one of a reading that asked
/// for the reset status (a 429, an outage that failed its plain retry too) also stops asking for an
/// hour, so a failing server is not asked twice at every poll. `asked`: see `ask_resets`.
fn merge(entry: &mut Cached, result: Option<Result<Fetched, String>>, asked: bool, now: DateTime<Utc>) {
    match result {
        Some(Ok(Fetched { windows, credits, resets, resets_withheld, resets_refused, monitor_error, billing, extra_usage })) => {
            let previous = std::mem::take(entry);
            // Billing is read less often than usage: keep the last record between reads.
            let billing = match (billing, previous.billing) {
                (Some(new), Some(old)) => Some(old.overlay(&new)),
                (new, old) => new.or(old),
            };
            let (resets, resets_read_at, resets_withheld) = match resets {
                Some(resets) => (resets, Some(now), resets_withheld),
                None => (unexpired(previous.resets, now), previous.resets_read_at, previous.resets_withheld),
            };
            *entry = Cached { windows, credits, resets, at: Some(now), blocked_until: None, error: None, monitor_error, billing, extra_usage,
                resets_read_at, resets_withheld,
                resets_tried_at: if asked { Some(now) } else { previous.resets_tried_at },
                resets_refused: if asked { resets_refused } else { previous.resets_refused },
                verified: true };
        }
        Some(Err(err)) => {
            let backoff = err.strip_prefix("rate_limited:").and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(MIN_BACKOFF).clamp(MIN_BACKOFF, 86400);
            entry.blocked_until = Some(now + chrono::Duration::seconds(backoff));
            if asked { entry.resets_tried_at = Some(now); }
            entry.error = Some(if err.starts_with("rate_limited") { "rate_limited".into() } else { err });
            entry.monitor_error = None;
            entry.verified = false;
        }
        None => {}
    }
}

/// The resets still to be used at `now`.
fn unexpired(resets: Vec<ResetGrant>, now: DateTime<Utc>) -> Vec<ResetGrant> {
    resets.into_iter().filter(|r| r.expires_at.is_none_or(|e| e > now)).collect()
}

/// Whether an account's billing record is due for another read: at most every twelve hours per
/// account and app run, since it changes about once a month.
pub(crate) fn billing_due(key: &str) -> bool {
    use std::time::{Duration, Instant};
    static LAST: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Instant>>> = std::sync::OnceLock::new();
    let mut last = LAST.get_or_init(Default::default).lock().unwrap();
    let due = last.get(key).map_or(true, |t| t.elapsed() >= Duration::from_secs(12 * 3600));
    if due { last.insert(key.to_string(), Instant::now()); }
    due
}

/// CLI logins the provider rejected, per account: a fingerprint of the access token that got the
/// 401. The account counts as rejected only while its saved login still has that token, so a
/// renewal by the CLI clears it without another request.
fn cli_rejections() -> &'static std::sync::Mutex<HashMap<String, u64>> {
    static REJECTED: std::sync::OnceLock<std::sync::Mutex<HashMap<String, u64>>> = std::sync::OnceLock::new();
    REJECTED.get_or_init(Default::default)
}

fn fingerprint(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut h);
    h.finish()
}

/// Notes how a usage read with the CLI's own access token went.
fn note_cli_login(slot: &Slot, token: &str, result: &Result<Fetched, String>) {
    let mut rejected = cli_rejections().lock().unwrap_or_else(|e| e.into_inner());
    match result {
        Err(e) if e == "token_invalid" => { rejected.insert(monitoring::key(slot), fingerprint(token)); }
        Ok(_) => { rejected.remove(&monitoring::key(slot)); }
        Err(_) => {}
    }
}

/// Whether the provider rejected this saved CLI login (see `note_cli_login`).
pub fn cli_rejected(slot: &Slot) -> bool {
    let rejected = cli_rejections().lock().unwrap_or_else(|e| e.into_inner());
    match (rejected.get(&monitoring::key(slot)), slot.access()) {
        (Some(bad), Some((token, _))) => *bad == fingerprint(&token),
        _ => false,
    }
}

pub fn alias_key(provider: Provider, id: &str) -> String {
    format!("{}:{id}", provider.name().to_lowercase())
}

struct Fetched {
    windows: Vec<UsageWindow>,
    credits: Option<String>,
    /// None: the reply carried no reset status (Claude, a reading that did not ask): keep the last.
    resets: Option<Vec<ResetGrant>>,
    /// Claude: the status said its resets are listed only to other clients; the ask was refused.
    resets_withheld: bool,
    resets_refused: bool,
    monitor_error: Option<String>,
    billing: Option<crate::model::BillingHints>,
    extra_usage: Option<crate::model::ExtraUsage>,
}

trait Usage {
    async fn fetch(&self, provider: Provider, token: &str, account: Option<&str>, ask_resets: bool, observe: &evidence::Observer<'_>) -> Result<Fetched, String>;
}
struct Api<'a>(&'a reqwest::Client, &'a Slot);
impl Usage for Api<'_> {
    async fn fetch(&self, provider: Provider, token: &str, account: Option<&str>, ask_resets: bool, observe: &evidence::Observer<'_>) -> Result<Fetched, String> {
        fetch_token(self.0, self.1, provider, token, account, ask_resets, observe).await
    }
}

async fn fetch_one(client: &reqwest::Client, slot: &Slot, monitor: &Monitor, active: bool, ask_resets: bool) -> Result<Fetched, String> {
    fetch_one_with(client, slot, monitor, active, ask_resets, &Api(client, slot)).await
}

async fn fetch_one_with(client: &reqwest::Client, slot: &Slot, monitor: &Monitor, active: bool, ask_resets: bool, usage: &impl Usage) -> Result<Fetched, String> {
    let result = fetch_session(client, slot, monitor, ask_resets, usage).await;
    // Only the currently captured CLI login may supply a fallback access token.
    // Read usage once; never renew or revoke that login, and never bypass a 429.
    match result {
        Err(error) if active && monitor.connected(slot) && !slot.expired() && !error.starts_with("rate_limited") => {
            if let Some((token, account)) = slot.access() {
                match observed_usage(usage, monitor, slot, &token, account.as_deref(), Origin::ActiveCliFallback, ask_resets).await {
                    Ok(mut live) => {
                    // Fresh telemetry stays eligible for balancing. Keep the monitor's
                    // separate error visible so its Reconnect control is still honest.
                    live.monitor_error = Some(error);
                    return Ok(live);
                    }
                    Err(error) if error.starts_with("rate_limited") => return Err(error),
                    Err(_) => {},
                }
            }
            Err(error)
        }
        result => result,
    }
}

async fn fetch_session(client: &reqwest::Client, slot: &Slot, monitor: &Monitor, ask_resets: bool, usage: &impl Usage) -> Result<Fetched, String> {
    let owned = monitor.connected(slot);
    let (token, account) = if owned {
        monitor.token(slot, client, None).await?
    } else {
        if slot.expired() { return Err("monitor_setup".into()); }
        slot.access().ok_or("monitor_setup")?
    };
    let origin = if owned { Origin::Monitoring } else { Origin::Cli };
    match observed_usage(usage, monitor, slot, &token, account.as_deref(), origin, ask_resets).await {
        Err(e) if e == "token_invalid" && owned => {
            let (renewed, account) = monitor.token(slot, client, Some(&token)).await?;
            // Exactly one renewal and one usage retry for the independent session.
            match observed_usage(usage, monitor, slot, &renewed, account.as_deref(), Origin::Monitoring, ask_resets).await {
                Err(e) if e == "token_invalid" => {
                    monitor.reject(slot, &renewed).await?;
                    Err("monitor_reconnect".into())
                }
                result => result,
            }
        }
        Err(e) if e == "token_invalid" => Err("monitor_setup".into()),
        result => result,
    }
}

async fn observed_usage(usage: &impl Usage, monitor: &Monitor, slot: &Slot, token: &str, account: Option<&str>, origin: Origin, ask_resets: bool) -> Result<Fetched, String> {
    let started = std::time::Instant::now();
    let observe = |evidence| monitor.record(slot, Event::ProviderEvidence { origin, evidence });
    let result = usage.fetch(slot.provider, token, account, ask_resets, &observe).await;
    if matches!(origin, Origin::Cli | Origin::ActiveCliFallback) {
        note_cli_login(slot, token, &result);
    }
    monitor.record(slot, Event::Usage { origin, code: Code::from_error(result.as_ref().err().map(String::as_str)), elapsed_ms: diagnostics::millis(started) });
    result
}

async fn fetch_token(client: &reqwest::Client, slot: &Slot, provider: Provider, token: &str, account: Option<&str>, ask_resets: bool, observe: &evidence::Observer<'_>) -> Result<Fetched, String> {
    match provider {
        Provider::Claude => claude::fetch(client, token, slot, ask_resets, observe)
            .await.map(|u| Fetched { windows: u.windows, credits: None, resets: u.resets, resets_withheld: u.withheld, resets_refused: u.refused,
                monitor_error: None, billing: u.billing, extra_usage: u.extra_usage }),
        Provider::Codex => codex::fetch(client, token, account, observe)
            .await.map(|u| Fetched { windows: u.windows, credits: u.credits, resets: Some(u.resets), resets_withheld: false, resets_refused: false,
                monitor_error: None, billing: u.billing, extra_usage: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{slot, TempDir};

    #[tokio::test]
    async fn repeated_monitor_usage_failure_and_cli_fallback_have_independent_diagnostics() {
        struct Observed(std::sync::Mutex<Vec<String>>);
        impl Usage for Observed {
            async fn fetch(&self, _: Provider, token: &str, _: Option<&str>, _: bool, _: &evidence::Observer<'_>) -> Result<Fetched, String> {
                self.0.lock().unwrap().push(token.to_owned());
                if token == "monitor-access" { return Err("bad_response".into()); }
                Ok(Fetched { windows: observed(24.0).windows, credits: None, resets: Some(vec![]), resets_withheld: false, resets_refused: false, monitor_error: None, billing: None, extra_usage: None })
            }
        }
        let dir = TempDir::new();
        let recorder = std::sync::Arc::new(crate::diagnostics::Recorder::new(dir.0.join("journal")));
        recorder.start();
        let monitor = Monitor::new(dir.0.join("monitoring")).with_diagnostics(recorder.clone());
        let account = slot("active:org", "live-cli-access", "cli-owned-refresh", i64::MAX);
        let login = slot("active:org", "monitor-access", "monitor-refresh", i64::MAX);
        monitor.connect(&account, &login, &[]).await.unwrap();
        let usage = Observed(std::sync::Mutex::new(vec![]));
        let client = crate::net::builder().build().unwrap();
        let original = serde_json::to_vec(&account).unwrap();
        for _ in 0..60 {
            let result = fetch_one_with(&client, &account, &monitor, true, false, &usage).await.unwrap();
            assert_eq!(result.windows[0].used_percent, 24.0);
            assert_eq!(result.monitor_error.as_deref(), Some("bad_response"));
        }
        let calls = usage.0.lock().unwrap();
        assert_eq!(calls.len(), 120);
        for pair in calls.chunks_exact(2) { assert_eq!(pair, ["monitor-access", "live-cli-access"]); }
        assert_eq!(serde_json::to_vec(&account).unwrap(), original);
        let report: serde_json::Value = serde_json::from_slice(&recorder.report(&crate::model::Snapshot::default(), &Settings::default()).unwrap()).unwrap();
        let events = report["events"].as_array().unwrap();
        let readings: Vec<_> = events.iter().filter(|r| r["event"]["type"] == "usage").collect();
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0]["event"]["origin"], "monitoring");
        assert_eq!(readings[0]["event"]["code"], "bad_response");
        assert_eq!(readings[1]["event"]["origin"], "active_cli_fallback");
        assert_eq!(readings[1]["event"]["code"], "ok");
        assert!(!events.iter().any(|r| r["event"]["type"] == "refresh"));
    }

    #[tokio::test]
    async fn only_active_cli_access_tokens_can_supply_fresh_usage_after_monitor_failure() {
        struct Observed(std::sync::Mutex<Vec<String>>);
        impl Usage for Observed {
            async fn fetch(&self, _: Provider, token: &str, _: Option<&str>, _: bool, _: &evidence::Observer<'_>) -> Result<Fetched, String> {
                self.0.lock().unwrap().push(token.to_owned());
                Ok(Fetched { windows: observed(24.0).windows, credits: None, resets: Some(vec![]), resets_withheld: false, resets_refused: false, monitor_error: None, billing: None, extra_usage: None })
            }
        }
        for active in [false, true] {
            let dir = TempDir::new();
            let recorder = std::sync::Arc::new(crate::diagnostics::Recorder::new(dir.0.join("journal")));
            recorder.start();
            let monitor = Monitor::new(dir.0.join("monitoring")).with_diagnostics(recorder.clone());
            let account = slot("active:org", "live-cli-access", "cli-owned-refresh", i64::MAX);
            let login = slot("active:org", "monitor-access", "monitor-refresh", i64::MAX);
            monitor.connect(&account, &login, &[]).await.unwrap();
            // Persist a rejected monitor, so any attempted automatic renewal fails
            // the test rather than hitting a real provider.
            monitor.token(&account, &crate::net::builder().build().unwrap(), None).await.unwrap();
            monitor.reject(&account, "monitor-access").await.unwrap();
            let usage = Observed(std::sync::Mutex::new(vec![]));
            let original = serde_json::to_vec(&account).unwrap();
            let result = fetch_one_with(&crate::net::builder().build().unwrap(), &account, &monitor, active, false, &usage).await;
            if active {
                let fetched = result.unwrap();
                assert_eq!(fetched.windows[0].used_percent, 24.0);
                assert_eq!(fetched.monitor_error.as_deref(), Some("monitor_reconnect"));
                assert_eq!(*usage.0.lock().unwrap(), ["live-cli-access"]);
            } else {
                assert_eq!(result.err().as_deref(), Some("monitor_reconnect"));
                assert!(usage.0.lock().unwrap().is_empty());
            }
            assert_eq!(serde_json::to_vec(&account).unwrap(), original);
            let report = String::from_utf8(recorder.report(&crate::model::Snapshot::default(), &Settings::default()).unwrap()).unwrap();
            for private in ["live-cli-access", "cli-owned-refresh", "monitor-access", "monitor-refresh", "active:org"] { assert!(!report.contains(private)); }
            assert_eq!(report.contains("active_cli_fallback"), active);
        }
    }

    fn observed(percent: f64) -> Cached {
        Cached {
            windows: vec![UsageWindow { kind: "session".into(), window_seconds: Some(18000), used_percent: percent,
                resets_at: Some(Utc::now() - chrono::Duration::hours(1)) }],
            at: Some(Utc::now() - chrono::Duration::hours(2)), verified: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn expired_imported_cli_session_requests_setup_without_refreshing_or_network_calls() {
        let dir = TempDir::new();
        let monitor = Monitor::new(dir.0.clone());
        let account = slot("inactive:org", "expired", "cli-owned-refresh", 0);
        let mut cache = Cache::new();
        let results = fetch_all(&crate::net::builder().build().unwrap(), &[account], &Active::new(), &mut cache, &Settings::default(), &monitor).await;
        assert_eq!(results[0].error.as_deref(), Some("monitor_setup"));
        assert!(results[0].stale);
        assert!(results[0].windows.is_empty());
        assert!(!results[0].monitor_connected);
        assert!(std::fs::read_dir(&dir.0).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn restart_and_expired_reset_keep_last_observed_usage_instead_of_inventing_zero() {
        let dir = TempDir::new();
        let monitor = Monitor::new(dir.0.clone());
        let account = slot("inactive:org", "expired", "cli-owned-refresh", 0);
        let original = observed(87.0);
        let measured_at = original.at;
        let original_reset = original.windows[0].resets_at;
        let mut before = Cache::new();
        before.insert(monitoring::key(&account), original);
        let bytes = serde_json::to_vec(&before).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("refresh"));
        let mut restarted: Cache = serde_json::from_slice(&bytes).unwrap();
        assert!(!restarted[&monitoring::key(&account)].verified);
        let results = fetch_all(&crate::net::builder().build().unwrap(), &[account], &Active::new(), &mut restarted, &Settings::default(), &monitor).await;
        assert!(results[0].stale);
        assert_eq!(results[0].windows[0].used_percent, 87.0);
        assert_eq!(results[0].windows[0].resets_at, original_reset);
        assert_eq!(results[0].fetched_at, measured_at);
    }

    #[tokio::test]
    async fn the_snapshot_says_when_monitoring_ends_and_whether_it_needs_a_new_sign_in() {
        let dir = TempDir::new();
        let monitor = Monitor::new(dir.0.clone());
        // One the provider refused after its end, one ending within days, one ending much later.
        let ended = Utc::now() - chrono::Duration::days(1);
        let soon = Utc::now() + chrono::Duration::days(2);
        let later = Utc::now() + chrono::Duration::days(20);
        let (mut accounts, mut cache) = (vec![], Cache::new());
        for (id, ends) in [("ended:org", ended), ("soon:org", soon), ("later:org", later)] {
            let cli = slot(id, "cli-access", &format!("cli-refresh-{id}"), 0);
            let mut login = slot(id, &format!("monitor-access-{id}"), &format!("monitor-refresh-{id}"),
                (Utc::now() + chrono::Duration::hours(1)).timestamp_millis());
            login.data["credentials"]["claudeAiOauth"]["refreshTokenExpiresAt"] = serde_json::json!(ends.timestamp_millis());
            monitor.connect(&cli, &login, &[]).await.unwrap();
            // Its last reading stands in for this refresh's: nothing goes to the network.
            let mut entry = observed(10.0);
            entry.blocked_until = Some(Utc::now() + chrono::Duration::hours(1));
            cache.insert(monitoring::key(&cli), entry);
            accounts.push(cli);
        }
        monitor.reject(&accounts[0], "monitor-access-ended:org").await.unwrap();
        let results = fetch_all(&crate::net::builder().build().unwrap(), &accounts, &Active::new(), &mut cache, &Settings::default(), &monitor).await;
        let by = |id: &str| results.iter().find(|a| a.id == id).unwrap();
        assert!(results.iter().all(|a| a.monitor_connected), "an ended sign-in is still saved, so still connected");
        use crate::model::RenewalKind;
        assert_eq!(by("ended:org").monitor_renewal, Some(RenewalKind::Ended));
        assert_eq!(by("ended:org").monitor_ends_at.map(|t| t.timestamp_millis()), Some(ended.timestamp_millis()));
        assert_eq!(by("soon:org").monitor_renewal, Some(RenewalKind::Ending), "ends within RENEW_DAYS");
        assert_eq!(by("later:org").monitor_renewal, None);
        assert_eq!(by("later:org").monitor_ends_at.map(|t| t.timestamp_millis()), Some(later.timestamp_millis()));
    }

    #[tokio::test]
    async fn the_snapshot_says_when_a_saved_cli_sign_in_ends_and_whether_one_not_in_use_needs_a_new_one() {
        let dir = TempDir::new();
        let monitor = Monitor::new(dir.0.clone());
        let ended = Utc::now() - chrono::Duration::days(1);
        let soon = Utc::now() + chrono::Duration::days(2);
        let later = Utc::now() + chrono::Duration::days(20);
        let (mut accounts, mut cache) = (vec![], Cache::new());
        for (id, ends) in [("ended:org", Some(ended)), ("soon:org", Some(soon)), ("later:org", Some(later)), ("unknown:org", None), ("in-use:org", Some(ended))] {
            let mut cli = slot(id, "cli-access", &format!("cli-refresh-{id}"), 0);
            if let Some(t) = ends {
                cli.data["credentials"]["claudeAiOauth"]["refreshTokenExpiresAt"] = serde_json::json!(t.timestamp_millis());
            }
            // Its last reading stands in for this refresh's: nothing goes to the network.
            let mut entry = observed(10.0);
            entry.blocked_until = Some(Utc::now() + chrono::Duration::hours(1));
            cache.insert(monitoring::key(&cli), entry);
            accounts.push(cli);
        }
        let mut active = Active::new();
        active.insert((Provider::Claude, "in-use:org".to_string()), vec!["Windows".into()]);
        let results = fetch_all(&crate::net::builder().build().unwrap(), &accounts, &active, &mut cache, &Settings::default(), &monitor).await;
        let by = |id: &str| results.iter().find(|a| a.id == id).unwrap();
        let ms = |t: Option<DateTime<Utc>>| t.map(|t| t.timestamp_millis());
        use crate::model::RenewalKind;
        assert_eq!((by("ended:org").cli_renewal, ms(by("ended:org").cli_ends_at)), (Some(RenewalKind::Ended), Some(ended.timestamp_millis())));
        assert_eq!(by("soon:org").cli_renewal, Some(RenewalKind::Ending), "ends within RENEW_DAYS");
        assert_eq!((by("later:org").cli_renewal, ms(by("later:org").cli_ends_at)), (None, Some(later.timestamp_millis())));
        assert_eq!((by("unknown:org").cli_renewal, by("unknown:org").cli_ends_at), (None, None), "an older Claude Code saved no end");
        let in_use = by("in-use:org");
        assert!(in_use.active);
        assert_eq!((in_use.cli_renewal, ms(in_use.cli_ends_at)), (None, Some(ended.timestamp_millis())), "the CLI manages the sign-in in use");
    }

    #[tokio::test]
    async fn cache_is_provider_scoped_and_backoff_preserves_error_and_timestamp() {
        let dir = TempDir::new();
        let monitor = Monitor::new(dir.0.clone());
        let claude = slot("same-id", "unused", "unused", 0);
        let mut codex = claude.clone();
        codex.provider = Provider::Codex;
        let mut cache = Cache::new();
        for (account, percent) in [(&claude, 87.0), (&codex, 12.0)] {
            let mut entry = observed(percent);
            entry.error = Some("rate_limited".into());
            entry.blocked_until = Some(Utc::now() + chrono::Duration::hours(1));
            cache.insert(monitoring::key(account), entry);
        }
        let results = fetch_all(&crate::net::builder().build().unwrap(), &[claude, codex], &Active::new(), &mut cache, &Settings::default(), &monitor).await;
        assert_eq!(results[0].windows[0].used_percent, 87.0);
        assert_eq!(results[1].windows[0].used_percent, 12.0);
        assert!(results.iter().all(|a| a.stale && a.error.as_deref() == Some("rate_limited")));
        assert_eq!(cache.len(), 2);
    }
    /// The owner's hold (decisions log, banked-resets session): SlopBar never tells Anthropic it is
    /// Claude Code to be answered what Claude withholds from it, never asks at a limit wall and never
    /// claims a reset. No code names Claude Code's client identity (a `claude-cli/` or `claude-code/`
    /// User-Agent, an `x-app` header), sets a User-Agent outside the Codex provider (which sends
    /// `codex-cli`, from before this rule), asks `at_wall` or calls `reset_rate_limits`; and, as an
    /// allowlist that a name built at run time cannot pass, the code that calls Anthropic sends no
    /// header but `anthropic-beta` and `Accept`, and POSTs only to renew and to revoke a monitoring
    /// sign-in.
    #[test]
    fn no_request_claims_to_be_claude_code_or_spends_a_reset() {
        fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() { sources(&path, out) } else if path.extension().is_some_and(|x| x == "rs") { out.push(path) }
            }
        }
        // A whole word: `at_wall` is found in `?at_wall=1`, not in `format_wall_time`.
        fn word(code: &str, needle: &str) -> bool {
            let part = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            code.match_indices(needle).any(|(i, _)| !part(code[..i].chars().next_back()) && !part(code[i + needle.len()..].chars().next()))
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![];
        sources(&src, &mut files);
        assert!(files.len() > 50, "the crate's sources");
        // Spelled in pieces, so that this test is not its own finding.
        let quoted = [concat!("claude", "-cli"), concat!("claude", "-code/"), concat!("\"x", "-app\""), concat!("\"user", "-agent\"")];
        let words = [concat!("at", "_wall"), concat!("reset", "_rate_limits"), concat!("user", "_agent"), concat!("default", "_headers")];
        for path in &files {
            let rel = path.strip_prefix(&src).unwrap().to_string_lossy().replace('\\', "/");
            let code = crate::source_guard::tokens(&std::fs::read_to_string(path).unwrap()).to_lowercase();
            let codex = rel == "providers/codex.rs";
            for needle in quoted.iter().filter(|n| !(codex && n.contains("agent"))) {
                assert!(!code.contains(needle), "{rel}: {needle} (the owner's hold on impersonating Claude Code)");
            }
            for needle in words.iter().filter(|n| !(codex && n.contains("agent"))) {
                assert!(!word(&code, needle), "{rel}: {needle} (the owner's hold on impersonating Claude Code)");
            }
        }
        for (rel, posts) in [("providers/claude.rs", 0), ("accounts.rs", 0), ("monitoring.rs", 2)] {
            let code = crate::source_guard::tokens(&std::fs::read_to_string(src.join(rel)).unwrap()).to_lowercase();
            for (i, _) in code.match_indices(".header(") {
                let arg = &code[i + ".header(".len()..];
                assert!(arg.starts_with("\"anthropic-beta\"") || arg.starts_with("\"accept\""), "{rel}: a header other than anthropic-beta or Accept: {}", arg.chars().take(40).collect::<String>());
            }
            assert!(code.match_indices(".headers(").all(|(i, _)| code[i + ".headers(".len()..].starts_with(')')), "{rel}: a header map set on a request");
            assert_eq!(code.matches(".post(").count(), posts, "{rel}: a POST to Anthropic other than renewing or revoking a monitoring sign-in");
        }
    }

    fn grant(kind: &str, expires_in_h: Option<i64>) -> ResetGrant {
        ResetGrant { kind: kind.into(), title: None, count: 1, usable: false, expires_at: expires_in_h.map(|h| Utc::now() + chrono::Duration::hours(h)), next_available_at: None }
    }

    fn claude(resets: Option<Vec<ResetGrant>>, withheld: bool, refused: bool, used: f64) -> Fetched {
        let mut windows = observed(used).windows;
        windows[0].used_percent = used;
        Fetched { windows, credits: None, resets, resets_withheld: withheld, resets_refused: refused, monitor_error: None, billing: None, extra_usage: None }
    }

    #[test]
    fn a_reading_without_the_reset_status_keeps_the_resets_last_read() {
        let now = Utc::now();
        let mut entry = Cached::default();
        merge(&mut entry, Some(Ok(claude(Some(vec![grant("fable", Some(48)), grant("full", Some(-1)), grant("weekly", None)]), false, false, 40.0))), true, now);
        assert_eq!((entry.resets.len(), entry.resets_read_at, entry.resets_tried_at), (3, Some(now), Some(now)));
        // A plain reading (it did not ask, or the server ignored the parameter) keeps them, less the expired one.
        let later = now + chrono::Duration::minutes(5);
        merge(&mut entry, Some(Ok(claude(None, false, false, 41.0))), false, later);
        let kinds: Vec<&str> = entry.resets.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!((kinds, entry.resets_read_at, entry.resets_tried_at), (vec!["fable", "weekly"], Some(now), Some(now)));
        // A failure keeps them too; one of a reading that asked stops asking for an hour.
        merge(&mut entry, Some(Err("http_500".into())), false, later);
        assert_eq!((entry.resets.len(), entry.resets_tried_at), (2, Some(now)));
        entry.windows[0].used_percent = 95.0;
        assert!(ask_resets(Provider::Claude, Some(&entry), later), "near a limit, with the last ask answered");
        merge(&mut entry, Some(Err("rate_limited:30".into())), true, later);
        assert_eq!((entry.resets.len(), entry.resets_tried_at, entry.error.as_deref()), (2, Some(later), Some("rate_limited")));
        assert!(!ask_resets(Provider::Claude, Some(&entry), later + chrono::Duration::minutes(30)), "the failed ask pauses even the near-limit asking");
        assert!(ask_resets(Provider::Claude, Some(&entry), later + chrono::Duration::minutes(61)));
        // A status that lists none clears them; one that says Claude withholds them says so.
        merge(&mut entry, Some(Ok(claude(Some(vec![]), true, false, 41.0))), true, later);
        assert!(entry.resets.is_empty() && entry.resets_withheld && !entry.resets_refused);
        // A refused ask keeps what was known and says it was refused.
        merge(&mut entry, Some(Ok(claude(None, false, true, 41.0))), true, later);
        assert!(entry.resets_withheld && entry.resets_refused);
        // A cache written before resets were kept (no new fields) still loads, and back again.
        let old: Cache = serde_json::from_str(r#"{"claude:x":{"windows":[],"credits":null,"resets":[],"at":null,"blocked_until":null,"error":null,"monitor_error":null,"billing":null,"extra_usage":null}}"#).unwrap();
        assert!(old["claude:x"].resets_tried_at.is_none() && !old["claude:x"].resets_withheld);
        let round: Cache = serde_json::from_slice(&serde_json::to_vec(&HashMap::from([("claude:x".to_string(), entry)])).unwrap()).unwrap();
        assert!(round["claude:x"].resets_withheld);
    }

    #[test]
    fn the_reset_status_is_asked_hourly_daily_while_withheld_and_near_a_limit_every_few_minutes() {
        let now = Utc::now();
        let ago = |m: i64| Some(now - chrono::Duration::minutes(m));
        let at = |used: f64, tried: Option<DateTime<Utc>>, read: Option<DateTime<Utc>>, withheld: bool, refused: bool| {
            let mut c = Cached { windows: observed(used).windows, resets_tried_at: tried, resets_read_at: read, resets_withheld: withheld, resets_refused: refused, ..Default::default() };
            c.windows[0].used_percent = used;
            c
        };
        assert!(!ask_resets(Provider::Codex, None, now), "Codex reads its credits with every reading");
        assert!(ask_resets(Provider::Claude, None, now), "never read");
        assert!(ask_resets(Provider::Claude, Some(&at(10.0, None, None, false, false)), now), "never asked");
        assert!(!ask_resets(Provider::Claude, Some(&at(10.0, ago(30), ago(30), false, false)), now));
        assert!(ask_resets(Provider::Claude, Some(&at(10.0, ago(61), ago(61), false, false)), now), "hourly");
        // Near a limit: every poll, at most every 5 minutes, while the last ask was answered.
        assert!(ask_resets(Provider::Claude, Some(&at(92.0, ago(6), ago(6), false, false)), now));
        assert!(!ask_resets(Provider::Claude, Some(&at(92.0, ago(2), ago(2), false, false)), now), "5-minute floor");
        assert!(!ask_resets(Provider::Claude, Some(&at(92.0, ago(6), ago(70), false, false)), now), "the last ask went unanswered");
        // Withheld or refused: daily, at a limit too.
        assert!(!ask_resets(Provider::Claude, Some(&at(100.0, ago(6 * 60), ago(6 * 60), true, false)), now));
        assert!(ask_resets(Provider::Claude, Some(&at(10.0, ago(25 * 60), ago(25 * 60), true, false)), now));
        assert!(!ask_resets(Provider::Claude, Some(&at(100.0, ago(23 * 60), ago(23 * 60), false, true)), now));
        // A clock set back is no reason to stop.
        assert!(ask_resets(Provider::Claude, Some(&at(10.0, Some(now + chrono::Duration::hours(3)), None, true, false)), now));
    }
}
