//! How much each of the user's computers uses, whatever account it is on, and which computers use
//! little enough to share one plan.
//!
//! SlopBar measures usage per account; a computer's use is worked out here by ceiling attribution,
//! safe by construction: a computer is charged every account weekly window it had in use at any
//! moment of that window (the CLI's sign-in here, from any source, WSL included; the desktop app's;
//! an account a project pin points to; an account leased here), whole: every rise of that window,
//! whichever computer it came from. Nothing is split between computers and no token counts are
//! used: two computers on one account are each charged all of it. That can only overstate a
//! computer's use, never understate it, and it is exact for a light computer that stays on its own
//! account.
//!
//! Each computer keeps its `Ledger`: the windows it had in use, each with its highest reading on
//! every UTC day; when each account was last in use here (a switch or a lease between two refreshes
//! counts too); the days it refreshed; and the days some use here could not be counted. Its busiest
//! 7 days are the most its windows rose, weighted by plan (`subscriptions::plan_weight`) and added
//! up, over any 8 UTC days in a row: any 7 days lie within 8, so a burst across two windows (one
//! account's weeks, or accounts used in turn) counts whole, overstated by a day at most. That
//! `Load` goes out in the computer's presence record (`presence::Presence::load`) while readings are
//! shared: totals only, never an account code or id. The other computers' loads are remembered
//! (`Peers`), and `advise` groups the computers measured long enough into the fewest plans that
//! carry them with 20% to spare.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::accounts::Provider;
use crate::model::{AccountUsage, Snapshot};
use crate::presence::IGNORE_AFTER;
use crate::settings::{Providers, Settings};
use crate::subscriptions::{self, END_READ_SECONDS, IDLE_SLACK_SECONDS, KINDS, POOL_BUFFER, SAME_WINDOW_SECONDS, WEEK_SECONDS};

/// 2: windows keep a reading per day. A ledger of another version starts over.
pub(crate) const VERSION: u8 = 2;
/// The `load.v` this SlopBar writes and reads; a load of any other is dropped.
const LOAD_VERSION: u32 = 1;
const FILE: &str = "machine-load.json";
/// The ledger file is read up to this size; `save` keeps it within.
const MAX_FILE: u64 = 256 * 1024;
/// Windows and days are kept this many weeks.
const KEEP_WEEKS: i64 = 12;
/// Beyond this many windows the oldest go, but never one a span of the lookback reaches.
const MAX_WINDOWS: usize = 360;
/// The busiest 7 days are looked for in spans ending on each day of this many latest weeks, and
/// the evidence in as many latest completed weeks (`subscriptions::POOL_WEEKS`).
const LOOK_WEEKS: i64 = subscriptions::POOL_WEEKS as i64;
/// A span is this many UTC days, so that any 7 days in a row lie within one.
const SPAN_DAYS: i64 = 8;
/// A window's readings cover at most this many UTC days (a week begun mid-day, and an hour's
/// jitter in its reset).
const DAYS_MAX: usize = 9;
/// A UTC week (Monday to Sunday) is measured when SlopBar refreshed here on this many days of it.
const MEASURED_DAYS: usize = 3;
/// A computer is measured with this many measured weeks among the latest `LOOK_WEEKS` completed
/// ones, the latest of them included.
pub const WEEKS_NEEDED: u8 = 4;
/// A weekly window resets within a week; a reset further ahead than this is no reading of one (a
/// clock set wrong then corrected, or another computer's reset dates).
const AHEAD_SECONDS: i64 = WEEK_SECONDS + 3600;
/// Every grouping is tried up to this many computers (as `advisor` does for plans); beyond it, the
/// busiest is placed first in the first group it fits.
const SEARCH_MAX: usize = 10;
/// The longest machine name kept in memory (`presence` cuts names at 63 characters already).
const NAME_MAX: usize = 63;
/// At most this many accounts in use are remembered of another computer, each code at most
/// `SEAT_MAX` characters (a presence code has 32).
const SEATS_MAX: usize = 64;
const SEAT_MAX: usize = 64;

// ---------------------------------------------------------------------------------------------
// The ledger: this computer's windows, accounts in use, refresh days and uncounted days

/// What this computer had in use and when it ran. Kept in `machine-load.json` in the app's folder,
/// never in backups. Accounts appear only as a code (`account_code`), never an id or email.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ledger {
    pub version: u8,
    /// Every account weekly window this computer had in use, with its readings by day.
    pub windows: Vec<Touched>,
    /// The UTC days SlopBar refreshed here with the provider on, by provider (`Provider::key`).
    pub days: BTreeMap<String, BTreeSet<NaiveDate>>,
    /// When each account was last in use here (`mark`), by `account_key`.
    pub used: BTreeMap<String, DateTime<Utc>>,
    /// The UTC days some use here could not be counted (`observe`), by provider.
    pub gaps: BTreeMap<String, BTreeSet<NaiveDate>>,
    /// The accounts in use on the UTC day being watched and whether each had a reading that day:
    /// one in use whose readings fail all day leaves that day a gap.
    pub today: Option<Watch>,
    /// The latest reset of a window left out of the file to keep it within `MAX_FILE`: use until
    /// then may be missing.
    pub trimmed: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Watch {
    pub day: NaiveDate,
    /// By `account_key`: whether a fresh reading of it came that day.
    pub read: BTreeMap<String, bool>,
}

/// One account weekly window this computer had in use. Short names keep a ledger of
/// `MAX_WINDOWS` windows well inside `MAX_FILE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Touched {
    #[serde(rename = "p")]
    pub provider: Provider,
    /// The account's code (`account_code`).
    #[serde(rename = "a")]
    pub account: String,
    /// "weekly", "weekly_opus" or "weekly_sonnet".
    #[serde(rename = "k")]
    pub kind: String,
    /// When the window resets: with the provider, account and kind, its identity (readings within
    /// `SAME_WINDOW_SECONDS` of it are the same window). It began a week before.
    #[serde(rename = "r")]
    pub resets_at: DateTime<Utc>,
    /// When its latest reading was taken.
    #[serde(rename = "t")]
    pub read_at: DateTime<Utc>,
    /// The plan's weight (`subscriptions::plan_weight`), the largest read; None while unknown.
    #[serde(rename = "w", default)]
    pub weight: Option<f64>,
    /// A Codex Pro 200 plan (`subscriptions::pro_200`): its windows begun before the allowance cut
    /// count against the new, smaller allowance.
    #[serde(rename = "x", default, skip_serializing_if = "std::ops::Not::not")]
    pub pro_200: bool,
    /// The highest share used read on each UTC day of the window, in percent: day 0 is the day it
    /// began. A day without a reading holds 0. Every reading after the window was first in use here
    /// counts, in use here or not since: the whole window is charged.
    #[serde(rename = "d", default)]
    pub daily: Vec<f64>,
}

impl Touched {
    fn began(&self) -> DateTime<Utc> {
        self.resets_at - Duration::seconds(WEEK_SECONDS)
    }

    fn first_day(&self) -> NaiveDate {
        self.began().date_naive()
    }

    /// The UTC day of its last moment.
    fn last_day(&self) -> NaiveDate {
        (self.resets_at - Duration::seconds(1)).date_naive()
    }

    fn peak(&self) -> f64 {
        self.daily.iter().copied().fold(0.0, f64::max)
    }

    /// Its highest reading by the end of `day`: 0 before it began, its peak after it ended.
    fn by(&self, day: NaiveDate) -> f64 {
        let n = (day - self.first_day()).num_days();
        if n < 0 { return 0.0; }
        self.daily.iter().take(n as usize + 1).copied().fold(0.0, f64::max)
    }

    fn read(&mut self, used: f64, at: DateTime<Utc>, weight: Option<f64>, pro_200: bool) {
        let day = (at.date_naive() - self.first_day()).num_days().clamp(0, DAYS_MAX as i64 - 1) as usize;
        if self.daily.len() <= day { self.daily.resize(day + 1, 0.0); }
        self.daily[day] = self.daily[day].max(stored_percent(used));
        self.read_at = self.read_at.max(whole_second(at));
        self.weight = match (self.weight, weight) { (Some(a), Some(b)) => Some(a.max(b)), (a, b) => a.or(b) };
        self.pro_200 |= pro_200;
    }
}

/// An account as the ledger names it: the first 16 hex digits of its presence code
/// (`presence::hash`), enough to tell a user's accounts apart, never its id or email.
fn account_code(provider: Provider, id: &str) -> String {
    crate::presence::hash(provider, id)[..16].to_string()
}

/// The provider and `account_code`, for what the ledger keeps per account.
fn account_key(provider: Provider, id: &str) -> String {
    format!("{}:{}", provider.key(), account_code(provider, id))
}

fn provider_of(key: &str) -> Option<Provider> {
    let (p, _) = key.split_once(':')?;
    Provider::ALL.into_iter().find(|x| x.key() == p)
}

pub fn load() -> Ledger {
    crate::settings::app_dir().map(|d| load_from(&d.join(FILE))).unwrap_or_default()
}

fn load_from(path: &Path) -> Ledger {
    let fresh = || Ledger { version: VERSION, ..Default::default() };
    if std::fs::metadata(path).map_or(true, |m| m.len() > MAX_FILE) { return fresh(); }
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice::<Ledger>(&b).ok())
        .filter(|l| l.version == VERSION).unwrap_or_else(fresh)
}

pub fn save(ledger: &Ledger) -> Result<(), String> {
    let dir = crate::settings::app_dir().ok_or("machine_load_storage")?;
    save_to(&dir.join(FILE), ledger)
}

fn save_to(path: &Path, ledger: &Ledger) -> Result<(), String> {
    let bytes = stored(ledger)?;
    crate::private_store::write(path, &bytes).map_err(|_| "machine_load_storage".into())
}

/// The ledger as written: should it not fit `MAX_FILE` (`MAX_WINDOWS` keeps that from happening in
/// practice), its oldest windows are left out and `trimmed` says until when, so the file always
/// reads back and use it no longer holds marks the computer uncounted rather than lighter.
fn stored(ledger: &Ledger) -> Result<Vec<u8>, String> {
    let mut kept = ledger.clone();
    kept.windows.sort_by_key(|t| t.resets_at);
    loop {
        let bytes = serde_json::to_vec(&kept).map_err(|_| "machine_load_storage")?;
        if bytes.len() as u64 <= MAX_FILE { return Ok(bytes); }
        // Days and accounts are bounded by `prune`: a ledger too large without windows is not one
        // `observe` made, and is not written.
        if kept.windows.is_empty() { return Err("machine_load_storage".into()); }
        let drop = (kept.windows.len() / 8).max(1);
        let latest = kept.windows[drop - 1].resets_at;
        kept.trimmed = Some(kept.trimmed.map_or(latest, |t| t.max(latest)));
        kept.windows.drain(..drop);
    }
}

fn enabled(providers: &Providers, provider: Provider) -> bool {
    match provider {
        Provider::Claude => providers.claude,
        Provider::Codex => providers.codex,
    }
}

/// A time without its fraction of a second, as the ledger keeps times.
fn whole_second(t: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(t.timestamp(), 0).unwrap_or(t)
}

/// A percent as the ledger keeps it: rounded up to a hundredth.
fn stored_percent(p: f64) -> f64 {
    (p * 100.0 - 1e-9).ceil() / 100.0 + 0.0
}

/// Notes that these accounts were in use here at `at`: an account switched to or from, leased or
/// released, or found in use by the last refresh. The next `observe` charges the window of each
/// that `at` falls in, though it is no longer in use by then: an account used and left between two
/// refreshes is charged all the same.
pub fn mark(ledger: &mut Ledger, accounts: &[(Provider, String)], at: DateTime<Utc>) {
    for (provider, id) in accounts {
        mark_key(ledger, &account_key(*provider, id), at);
    }
}

fn mark_key(ledger: &mut Ledger, key: &str, at: DateTime<Utc>) {
    let last = ledger.used.entry(key.to_string()).or_insert(at);
    *last = (*last).max(at);
}

fn gap(ledger: &mut Ledger, provider: Provider, day: NaiveDate) {
    ledger.gaps.entry(provider.key().to_string()).or_default().insert(day);
}

/// Ends the day watched for readings that failed all day, once `today` is another: each account in
/// use then without one fresh reading leaves that day a gap of its provider.
fn close_watch(ledger: &mut Ledger, today: NaiveDate) {
    let Some(watch) = ledger.today.take_if(|w| w.day != today) else { return };
    for key in watch.read.iter().filter(|(_, read)| !**read).map(|(k, _)| k) {
        if let Some(provider) = provider_of(key) { gap(ledger, provider, watch.day); }
    }
}

/// Folds one refresh into the ledger. The day counts as refreshed for every provider on. Each
/// account in use here (`AccountUsage::active` or `desktop`, or in `wanted`: pinned to a project or
/// leased here) is marked in use now (`mark`); each weekly window of an account whose last time in
/// use falls in it is touched, and a touched window keeps its highest reading of each day from then
/// on, whether or not the account is still in use here. Readings come from this computer's own read
/// of the account, a stale one included (an earlier reading of the same window, counted on the day
/// it was taken), and another computer's newer one shown here (`AccountUsage::shared`), which only
/// raises a window already touched. Not touched: a window not used yet (0% with its reset a full
/// week ahead, no fixed reset yet), and one resetting further ahead than `AHEAD_SECONDS`.
///
/// Use that cannot be counted leaves the day a gap of the provider (`ProviderLoad::uncounted`): the
/// desktop app here signed in to an account SlopBar does not have, an account in use with limits of
/// another length than a week, or one in use whose readings fail all day.
pub fn observe(ledger: &mut Ledger, snapshot: &Snapshot, wanted: &[(Provider, String)], providers: &Providers, now: DateTime<Utc>) {
    ledger.version = VERSION;
    let today = now.date_naive();
    close_watch(ledger, today);
    for p in Provider::ALL.into_iter().filter(|p| enabled(providers, *p)) {
        ledger.days.entry(p.key().to_string()).or_default().insert(today);
    }
    for acc in snapshot.accounts.iter().filter(|a| enabled(providers, a.provider)) {
        let key = account_key(acc.provider, &acc.id);
        let in_use = acc.active || acc.desktop || wanted.iter().any(|(p, id)| *p == acc.provider && *id == acc.id);
        // `desktop_apps` names no account for the desktop app here when SlopBar does not have it.
        let mut uncounted = acc.seats.as_ref().is_some_and(|s| s.desktop.is_none());
        if in_use {
            mark_key(ledger, &key, now);
            let watch = ledger.today.get_or_insert_with(|| Watch { day: today, read: BTreeMap::new() });
            *watch.read.entry(key.clone()).or_insert(false) |= !acc.stale && acc.error.is_none();
            uncounted |= acc.windows.iter().any(|w| w.kind == "window" || KINDS.contains(&w.kind.as_str()) && w.window_seconds.is_some_and(|s| s != WEEK_SECONDS));
        }
        if uncounted { gap(ledger, acc.provider, today); }
        let last_used = ledger.used.get(&key).copied();
        let account = account_code(acc.provider, &acc.id);
        let weight = subscriptions::plan_weight(acc);
        let pro_200 = subscriptions::pro_200(acc);
        let own = acc.fetched_at.unwrap_or(now).min(now);
        let readings = acc.windows.iter().map(|w| (w, own, false))
            .chain(acc.shared.iter().flat_map(|s| s.windows.iter().map(move |w| (w, s.taken_at.min(now), true))));
        for (w, read_at, shared) in readings {
            if !KINDS.contains(&w.kind.as_str()) || w.window_seconds.is_some_and(|s| s != WEEK_SECONDS) || !w.used_percent.is_finite() { continue; }
            let Some(resets_at) = w.resets_at.filter(|r| *r > now && (*r - now).num_seconds() <= AHEAD_SECONDS) else { continue };
            let used = w.used_percent.max(0.0);
            if used <= 0.0 && (resets_at - now).num_seconds() > WEEK_SECONDS - IDLE_SLACK_SECONDS { continue; }
            let began = resets_at - Duration::seconds(WEEK_SECONDS);
            let same = ledger.windows.iter_mut().find(|t| t.provider == acc.provider && t.account == account && t.kind == w.kind
                && (t.resets_at - resets_at).num_seconds().abs() < SAME_WINDOW_SECONDS);
            match same {
                Some(t) => t.read(used, read_at, weight, pro_200),
                None if !shared && (in_use || last_used.is_some_and(|u| began <= u && u < resets_at)) => {
                    let mut t = Touched { provider: acc.provider, account: account.clone(), kind: w.kind.clone(), resets_at: whole_second(resets_at),
                        read_at: whole_second(read_at), weight, pro_200, daily: vec![] };
                    t.read(used, read_at, weight, pro_200);
                    ledger.windows.push(t);
                }
                None => {}
            }
        }
    }
    prune(ledger, now);
}

/// The first UTC day a span of the lookback covers: spans end on each day of the latest
/// `LOOK_WEEKS` weeks, today included, and reach `SPAN_DAYS - 1` days back.
fn lookback(now: DateTime<Utc>) -> NaiveDate {
    now.date_naive() - Duration::days(LOOK_WEEKS * 7 - 1 + SPAN_DAYS - 1)
}

/// Drops what is older than `KEEP_WEEKS`, windows resetting further ahead than `AHEAD_SECONDS`
/// (a clock that was wrong), marks no window can hold any more, then beyond `MAX_WINDOWS` the oldest
/// windows, never one a span of the lookback reaches.
fn prune(ledger: &mut Ledger, now: DateTime<Utc>) {
    let since = now - Duration::weeks(KEEP_WEEKS);
    let ahead = now + Duration::seconds(AHEAD_SECONDS);
    ledger.windows.retain(|t| t.resets_at > since && t.resets_at <= ahead);
    if ledger.windows.len() > MAX_WINDOWS {
        let first = lookback(now);
        ledger.windows.sort_by_key(|t| t.resets_at);
        let spare = ledger.windows.iter().take_while(|t| t.last_day() < first).count();
        let drop = (ledger.windows.len() - MAX_WINDOWS).min(spare);
        ledger.windows.drain(..drop);
    }
    let first_day = since.date_naive();
    for days in ledger.days.values_mut().chain(ledger.gaps.values_mut()) {
        days.retain(|d| *d >= first_day);
    }
    let known = |k: &String| Provider::ALL.iter().any(|p| p.key() == k);
    ledger.days.retain(|k, d| !d.is_empty() && known(k));
    ledger.gaps.retain(|k, d| !d.is_empty() && known(k));
    // A window touched from now on resets after now, so it began less than a week ago.
    ledger.used.retain(|_, t| *t > now - Duration::seconds(WEEK_SECONDS) && *t <= ahead);
    if ledger.trimmed.is_some_and(|t| t <= since) { ledger.trimmed = None; }
}

// ---------------------------------------------------------------------------------------------
// A computer's load, as its presence record carries it

/// A computer's use as its presence record carries it (`presence::Presence::load`): per provider,
/// totals only, in plans of the provider's reference plan (`subscriptions::reference_plan`, weight
/// 1), rounded up to 0.01 plan; never an account code or id.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Load {
    pub v: u32,
    /// When it was worked out, by the writer's clock (a reader dates it no later than the record).
    pub at: DateTime<Utc>,
    /// By `Provider::key`, the providers on there.
    pub providers: BTreeMap<String, ProviderLoad>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProviderLoad {
    /// The busiest 7 days per weekly window kind, in plans (`provider_load`); a kind it had no
    /// window of in use is missing, which is no use of it.
    pub busiest: BTreeMap<String, f64>,
    /// The same for the 8 days to today, the windows still running at their pace.
    pub live: BTreeMap<String, f64>,
    /// Measured weeks among the latest `LOOK_WEEKS` completed ones (`evidence`).
    pub weeks: u8,
    pub measured: bool,
    /// Some use in the lookback is not in the numbers: on a plan of unknown weight (a free plan, an
    /// unknown tier), or on a day with a gap (`observe`), or left out of the file (`stored`).
    pub uncounted: bool,
}

/// An amount of plans as a load carries it: rounded up to 0.01 plan, so rounding never makes a
/// computer look lighter, and within 0 to 100 plans. Not a number reads as the most.
fn plans(v: f64) -> f64 {
    if v.is_nan() { return 100.0; }
    ((v * 100.0 - 1e-9).ceil() / 100.0).clamp(0.0, 100.0) + 0.0
}

impl Load {
    /// A `load` as another computer's record carries it, read leniently: None unless it is an
    /// object of `LOAD_VERSION` with its time. Within it, a provider this SlopBar does not know is
    /// left out, and so is one whose entry it cannot rely on (`provider_entry`): its use is then
    /// unknown, never none.
    pub fn from_value(raw: &serde_json::Value) -> Option<Load> {
        if raw.get("v")?.as_u64()? != LOAD_VERSION as u64 { return None; }
        let at = raw.get("at")?.as_str()?.parse::<DateTime<Utc>>().ok()?;
        let mut providers = BTreeMap::new();
        for (key, entry) in raw.get("providers").and_then(|p| p.as_object()).into_iter().flatten() {
            if !Provider::ALL.iter().any(|p| p.key() == key) { continue; }
            if let Some(entry) = provider_entry(entry) { providers.insert(key.clone(), entry); }
        }
        Some(Load { v: LOAD_VERSION, at, providers })
    }
}

/// One provider's entry of a peer's load, or None when it says nothing to rely on: no `busiest`
/// object, a `live` that is not one, or a known kind whose value is not a number of plans (0 or
/// more). Kinds this SlopBar does not know are left out. Without a `measured` it is not measured,
/// and without an `uncounted` some of its use is taken as uncounted.
fn provider_entry(entry: &serde_json::Value) -> Option<ProviderLoad> {
    let entry = entry.as_object()?;
    let kinds = |field: &str, required: bool| -> Option<BTreeMap<String, f64>> {
        let Some(map) = entry.get(field) else { return (!required).then(BTreeMap::new) };
        let mut out = BTreeMap::new();
        for (kind, v) in map.as_object()? {
            if !KINDS.contains(&kind.as_str()) { continue; }
            out.insert(kind.clone(), plans(v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)?));
        }
        Some(out)
    };
    Some(ProviderLoad {
        busiest: kinds("busiest", true)?,
        live: kinds("live", false)?,
        weeks: entry.get("weeks").and_then(|w| w.as_u64()).map_or(0, |w| w.min(LOOK_WEEKS as u64) as u8),
        measured: entry.get("measured").and_then(|m| m.as_bool()).unwrap_or(false),
        uncounted: entry.get("uncounted").and_then(|m| m.as_bool()).unwrap_or(true),
    })
}

/// What this computer's presence record carries: its load while readings are shared
/// (`Settings::share_readings`), else none.
pub fn published(ledger: &Ledger, settings: &Settings, now: DateTime<Utc>) -> Option<Load> {
    settings.share_readings.then(|| summary(ledger, &settings.providers, now))
}

/// This computer's load for every provider on.
pub fn summary(ledger: &Ledger, providers: &Providers, now: DateTime<Utc>) -> Load {
    let providers = Provider::ALL.into_iter().filter(|p| enabled(providers, *p))
        .map(|p| (p.key().to_string(), provider_load(ledger, p, now)))
        .collect();
    Load { v: LOAD_VERSION, at: whole_second(now), providers }
}

/// `percent` of window `t`'s plan in plans of the reference plan: times its weight; for a Codex Pro
/// 200 window begun before the allowance cut, against the new allowance
/// (`subscriptions::PRO_200_NEW_ALLOWANCE`, about double), as `subscriptions::busiest` counts it. A
/// plan of unknown weight counts nothing (`ProviderLoad::uncounted`).
fn in_plans(t: &Touched, percent: f64) -> f64 {
    let Some(weight) = t.weight else { return 0.0 };
    let percent = if t.pro_200 && t.began() < subscriptions::pro_200_cut() { percent / subscriptions::PRO_200_NEW_ALLOWANCE } else { percent };
    percent / 100.0 * weight
}

/// A provider's busiest 7 days per kind: for spans of `SPAN_DAYS` UTC days ending on each day of
/// the latest `LOOK_WEEKS` weeks, how much every window of that kind rose in the span (its highest
/// reading by the span's last day less its highest by the day before the span began, 0 before it
/// began), in plans and added up; the largest. `live` is the span ending today with each window
/// still running at its use so far, or its pace once three days have passed
/// (`subscriptions::live_week`); it counts toward the busiest too.
fn provider_load(ledger: &Ledger, provider: Provider, now: DateTime<Utc>) -> ProviderLoad {
    let today = now.date_naive();
    let first = lookback(now);
    let mine: Vec<&Touched> = ledger.windows.iter().filter(|t| t.provider == provider && t.last_day() >= first).collect();
    let gaps = ledger.gaps.get(provider.key()).is_some_and(|g| g.range(first..).next().is_some());
    let trimmed = ledger.trimmed.is_some_and(|t| t.date_naive() >= first);
    let mut out = ProviderLoad { uncounted: gaps || trimmed || mine.iter().any(|t| t.weight.is_none()), ..Default::default() };
    for kind in KINDS {
        let of_kind: Vec<&Touched> = mine.iter().copied().filter(|t| t.kind == kind).collect();
        if of_kind.is_empty() { continue; }
        let rise = |t: &Touched, end: f64, end_day: NaiveDate| in_plans(t, (end - t.by(end_day - Duration::days(SPAN_DAYS))).max(0.0));
        let span = |end_day: NaiveDate| of_kind.iter().map(|t| rise(t, t.by(end_day), end_day)).sum::<f64>();
        let live = of_kind.iter().map(|t| {
            let end = subscriptions::live_week(t.peak(), t.resets_at, t.read_at, now).unwrap_or_else(|| t.by(today));
            rise(t, end, today)
        }).sum::<f64>();
        let busiest = (1..LOOK_WEEKS * 7).map(|back| span(today - Duration::days(back))).fold(live, f64::max);
        out.busiest.insert(kind.to_string(), plans(busiest));
        out.live.insert(kind.to_string(), plans(live));
    }
    (out.weeks, out.measured) = evidence(ledger.days.get(provider.key()), &mine, now);
    out
}

fn monday(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

/// The measured weeks among the latest `LOOK_WEEKS` completed UTC weeks (Monday to Sunday): those
/// SlopBar refreshed here on `MEASURED_DAYS` days or more, every window this computer had in use
/// that ended in them read within `END_READ_SECONDS` of its reset (use after the last reading is
/// missing from it). Measured with `WEEKS_NEEDED` of them, the latest completed week one of them:
/// use while SlopBar was not running here goes unseen, so a computer is judged only on weeks it
/// watched.
fn evidence(days: Option<&BTreeSet<NaiveDate>>, windows: &[&Touched], now: DateTime<Utc>) -> (u8, bool) {
    let this_week = monday(now.date_naive());
    let measured: Vec<bool> = (1..=LOOK_WEEKS).map(|i| {
        let start = this_week - Duration::weeks(i);
        let end = start + Duration::days(7);
        let refreshed = days.map_or(0, |d| d.range(start..end).count()) >= MEASURED_DAYS;
        let ends_read = windows.iter().filter(|t| (start..end).contains(&t.last_day()))
            .all(|t| (t.resets_at - t.read_at).num_seconds() <= END_READ_SECONDS);
        refreshed && ends_read
    }).collect();
    let weeks = measured.iter().filter(|m| **m).count() as u8;
    (weeks, weeks >= WEEKS_NEEDED && measured[0])
}

// ---------------------------------------------------------------------------------------------
// The other computers' loads

/// The other computers' last loads, by machine id, in memory only. A record without a load from a
/// computer that quit (the empty record it leaves) says nothing about its use: its last load
/// stands, so a computer that is off is never read as idle. A running computer with accounts in
/// use whose record comes without one (an older SlopBar, readings no longer shared there) has its
/// use unknown from then on. A load stands for `presence::IGNORE_AFTER` from when it was worked out
/// (dated no later than its record), and everything is forgotten when sharing moves to another
/// place.
#[derive(Debug, Default)]
pub struct Peers {
    place: Option<crate::sharing::Place>,
    by_id: HashMap<String, Remembered>,
}

#[derive(Debug, Clone)]
struct Remembered {
    machine: String,
    /// When its newest record was written, as presence trusts it.
    seen_at: DateTime<Utc>,
    load: Option<Load>,
    /// Its accounts in use (provider, presence code), as of its newest record that said: computers
    /// already sharing an account are kept together. Kept only while its record is listed.
    seats: Vec<(Provider, String)>,
}

impl Peers {
    /// Takes what a read of `place` brought of every other computer (`presence::peers`, records the
    /// read missed included); `listed` says the read listed the place, so a computer missing from
    /// it no longer has a record there.
    pub fn note(&mut self, place: &crate::sharing::Place, peers: &[crate::presence::Peer], listed: bool, now: DateTime<Utc>) {
        if !self.at(place) {
            self.by_id.clear();
            self.place = Some(place.clone());
        }
        for p in peers {
            let e = self.by_id.entry(p.machine_id.clone())
                .or_insert_with(|| Remembered { machine: String::new(), seen_at: p.seen_at, load: None, seats: vec![] });
            e.machine = clean_name(&p.machine);
            e.seen_at = p.seen_at;
            let seats = || p.seats.iter().take(SEATS_MAX).map(|s| (s.p, s.h.chars().take(SEAT_MAX).collect())).collect();
            match &p.load {
                Some(load) => {
                    e.load = Some(Load { at: load.at.min(p.seen_at), ..load.clone() });
                    e.seats = seats();
                }
                None if p.live && !p.seats.is_empty() => {
                    e.load = None;
                    e.seats = seats();
                }
                None => {}
            }
        }
        if listed {
            for (id, e) in self.by_id.iter_mut() {
                if !peers.iter().any(|p| p.machine_id == *id) { e.seats.clear(); }
            }
        }
        for e in self.by_id.values_mut() {
            if e.load.as_ref().is_some_and(|l| (now - l.at).num_seconds() > IGNORE_AFTER) { e.load = None; }
        }
        // A load is dated no later than its record, so one still standing has its record listed.
        self.by_id.retain(|_, e| !e.machine.is_empty() && (now - e.seen_at).num_seconds() <= IGNORE_AFTER);
    }

    /// Whether these are the computers of `place` (the same folder or remote).
    pub fn at(&self, place: &crate::sharing::Place) -> bool {
        self.place.as_ref().is_some_and(|p| p.same_location(place))
    }
}

/// A name from another computer, without what does not show (`roster::invisible`) or control
/// characters, trimmed and bounded (presence cleans the names it reads the same way).
fn clean_name(name: &str) -> String {
    let visible: String = name.chars().filter(|c| !c.is_control() && !crate::roster::invisible(*c)).collect();
    visible.trim().chars().take(NAME_MAX).collect()
}

/// This computer's accounts in use as its presence record lists them (`presence::own`).
fn own_seats(accounts: &[AccountUsage]) -> Vec<(Provider, String)> {
    accounts.iter().filter(|a| (a.active || a.desktop) && !a.id.starts_with("token:"))
        .map(|a| (a.provider, crate::presence::hash(a.provider, &a.id))).collect()
}

// ---------------------------------------------------------------------------------------------
// Advice: which computers could share one plan

#[derive(Debug, Clone, Serialize)]
pub struct MachineAdvice {
    pub provider: Provider,
    /// The plan the percentages count in (`unit`): "one account".
    pub unit: String,
    /// Measured weeks a computer needs (`WEEKS_NEEDED`).
    pub weeks_needed: u8,
    /// This computer first, then the others by name.
    pub computers: Vec<Computer>,
    /// Computers that could share one plan of `unit`, two or more each.
    pub groups: Vec<MachineGroup>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Computer {
    pub machine: String,
    pub here: bool,
    /// Its busiest 7 days (`ProviderLoad::busiest`) in the weekly limit where they come to most, in
    /// percent of one `unit`; None when its use is unknown (no load from it for this provider).
    pub busiest_percent: Option<f64>,
    /// The same for the 8 days to today, the windows still running at their pace.
    pub live_percent: Option<f64>,
    pub weeks: u8,
    pub measured: bool,
    /// Measured, and its busiest 7 days plus 20% do not fit one `unit` in some weekly limit.
    pub needs_own: bool,
    /// Some of its use is not in the numbers (`ProviderLoad::uncounted`), or another SlopBar under
    /// its name (another Windows profile) did not say: listed, never grouped.
    pub uncounted: bool,
    /// When its newest record was written (this computer: now).
    pub seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineGroup {
    pub machines: Vec<String>,
    /// Their busiest 7 days added up, in the weekly limit where that comes to most, in percent of
    /// one `unit`.
    pub combined_percent: f64,
}

/// "One account": the provider's reference plan (`subscriptions::reference_plan`) when the user
/// holds one that renews, else the largest tier held (renewing ones first), with its weight. A plan
/// renews unless the provider says it ends or the user is letting it lapse (`cancel_after`). The
/// reference plan when nothing with a known weight is held.
fn unit(provider: Provider, accounts: &[AccountUsage], settings: &Settings, now: DateTime<Utc>) -> (String, f64) {
    let reference = subscriptions::reference_plan(provider);
    let held: Vec<(String, f64, bool)> = accounts.iter().filter(|a| a.provider == provider).filter_map(|a| {
        let lapsing = settings.subscriptions.get(&subscriptions::pref_key(a.provider, &a.id)).is_some_and(|p| p.cancel_after.is_some());
        Some((subscriptions::plan_label(a)?, subscriptions::plan_weight(a)?, subscriptions::ends_at(a, now).is_none() && !lapsing))
    }).collect();
    if held.iter().any(|(label, _, renews)| *renews && label == reference) { return (reference.to_string(), 1.0); }
    let largest = |renewing: bool| held.iter().filter(|(_, _, r)| *r || !renewing).max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
    largest(true).or_else(|| largest(false)).map_or((reference.to_string(), 1.0), |(label, w, _)| (label.clone(), *w))
}

/// One computer by name: every record under it (another Windows profile on the same PC, this
/// computer's own included), each with its load or none.
struct Named<'a> {
    machine: String,
    here: bool,
    seen_at: DateTime<Utc>,
    loads: Vec<Option<&'a Load>>,
    seats: Vec<(Provider, String)>,
}

/// Loads of one name added up for a provider: each kind's busiest and live summed, the fewest
/// weeks, measured only if every one is, uncounted if any is or any record has no load for it.
/// None when none has.
fn merged(parts: &[Option<&ProviderLoad>]) -> Option<ProviderLoad> {
    if let [Some(one)] = parts { return Some((*one).clone()); }
    let known: Vec<&ProviderLoad> = parts.iter().flatten().copied().collect();
    if known.is_empty() { return None; }
    let unknown = known.len() < parts.len();
    let sum = |of: &dyn Fn(&ProviderLoad) -> &BTreeMap<String, f64>| {
        let mut out: BTreeMap<String, f64> = BTreeMap::new();
        for (k, v) in known.iter().flat_map(|l| of(l)) { *out.entry(k.clone()).or_insert(0.0) += v; }
        out.into_iter().map(|(k, v)| (k, plans(v))).collect()
    };
    Some(ProviderLoad {
        busiest: sum(&|l| &l.busiest),
        live: sum(&|l| &l.live),
        weeks: known.iter().map(|l| l.weeks).min().unwrap_or(0),
        measured: !unknown && known.iter().all(|l| l.measured),
        uncounted: unknown || known.iter().any(|l| l.uncounted),
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0 + 0.0
}

/// For every provider on with use on some computer: each computer with what it uses in percent of
/// one plan of `unit`, and the computers that could share one. `own` is this computer's load and
/// `here` its name; `accounts` are this computer's (for `unit` and the accounts in use here).
/// Records under one name add up (`merged`). Groups are made of measured computers with all their
/// use counted whose busiest 7 days plus 20% fit one plan in every weekly limit, packed into the
/// fewest plans so that each group's busiest 7 days added up (each computer's own busiest, added
/// whatever their dates: conservative) plus 20% fits one plan in every weekly limit. Every grouping
/// is tried up to `SEARCH_MAX` computers, then the busiest first into the first group it fits; of
/// equally few plans, the one keeping most computers that already share an account together.
pub fn advise(own: &Load, here: &str, peers: &Peers, accounts: &[AccountUsage], settings: &Settings, now: DateTime<Utc>) -> Vec<MachineAdvice> {
    let here = clean_name(here);
    let mut named = vec![Named { machine: here.clone(), here: true, seen_at: now, loads: vec![Some(own)], seats: own_seats(accounts) }];
    let mut others: Vec<&Remembered> = peers.by_id.values().collect();
    others.sort_by(|a, b| a.machine.to_lowercase().cmp(&b.machine.to_lowercase()).then_with(|| a.machine.cmp(&b.machine)).then_with(|| a.seen_at.cmp(&b.seen_at)));
    for e in others {
        let load = e.load.as_ref().filter(|l| (now - l.at).num_seconds() <= IGNORE_AFTER);
        match named.iter_mut().find(|n| n.machine.eq_ignore_ascii_case(&e.machine)) {
            Some(n) => {
                n.loads.push(load);
                if !n.here { n.seen_at = n.seen_at.max(e.seen_at); }
                n.seats.extend(e.seats.iter().cloned());
            }
            None => named.push(Named { machine: e.machine.clone(), here: false, seen_at: e.seen_at, loads: vec![load], seats: e.seats.clone() }),
        }
    }
    let mut out = vec![];
    for provider in Provider::ALL.into_iter().filter(|p| enabled(&settings.providers, *p)) {
        let key = provider.key();
        let loads: Vec<Option<ProviderLoad>> = named.iter()
            .map(|n| merged(&n.loads.iter().map(|l| l.and_then(|l| l.providers.get(key))).collect::<Vec<_>>())).collect();
        // Shown only where some computer used the provider at all.
        if !loads.iter().flatten().any(|l| l.busiest.values().any(|v| *v > 0.0)) { continue; }
        let seats: Vec<Vec<&String>> = named.iter().map(|n| n.seats.iter().filter(|(p, _)| *p == provider).map(|(_, h)| h).collect()).collect();
        let (unit, weight) = unit(provider, accounts, settings, now);
        // Each computer's use per kind, in plans of `unit`.
        let shares = |of: &BTreeMap<String, f64>| -> Vec<f64> { KINDS.iter().map(|k| of.get(*k).copied().unwrap_or(0.0) / weight).collect() };
        let fits_one = |loads: &[f64]| loads.iter().all(|v| v * (1.0 + POOL_BUFFER) <= 1.0 + 1e-9);
        let mut computers = vec![];
        let mut members: Vec<(usize, Vec<f64>)> = vec![];
        for (i, (n, load)) in named.iter().zip(&loads).enumerate() {
            let busiest = load.as_ref().map(|l| shares(&l.busiest));
            let live = load.as_ref().map(|l| shares(&l.live));
            let measured = load.as_ref().is_some_and(|l| l.measured);
            let needs_own = measured && busiest.as_ref().is_some_and(|b| !fits_one(b));
            // Use missing from its numbers may make them read low: listed, never grouped.
            let uncounted = load.as_ref().is_some_and(|l| l.uncounted);
            if measured && !needs_own && !uncounted { members.push((i, busiest.clone().unwrap_or_default())); }
            let most = |v: &Vec<f64>| round2(v.iter().copied().fold(0.0, f64::max) * 100.0);
            computers.push(Computer {
                machine: n.machine.clone(),
                here: n.here,
                busiest_percent: busiest.as_ref().map(most),
                live_percent: live.as_ref().map(most),
                weeks: load.as_ref().map_or(0, |l| l.weeks),
                measured,
                needs_own,
                uncounted,
                seen_at: n.seen_at,
            });
        }
        let member_loads: Vec<Vec<f64>> = members.iter().map(|(_, l)| l.clone()).collect();
        let shared: Vec<Vec<bool>> = members.iter().map(|(a, _)| members.iter()
            .map(|(b, _)| a != b && seats[*a].iter().any(|h| seats[*b].contains(h))).collect()).collect();
        let mut groups: Vec<MachineGroup> = pack(&member_loads, &shared).into_iter().filter(|g| g.len() > 1).map(|mut g| {
            g.sort_unstable();
            let combined = (0..KINDS.len()).map(|k| g.iter().map(|&m| member_loads[m][k]).sum::<f64>()).fold(0.0, f64::max);
            MachineGroup { machines: g.iter().map(|&m| named[members[m].0].machine.clone()).collect(), combined_percent: round2(combined * 100.0) }
        }).collect();
        groups.sort_by_key(|g| computers.iter().position(|c| g.machines.first() == Some(&c.machine)));
        out.push(MachineAdvice { provider, unit, weeks_needed: WEEKS_NEEDED, computers, groups });
    }
    out
}

/// Whether `members` added up, plus `POOL_BUFFER`, fit one plan in every weekly limit.
fn fits(loads: &[Vec<f64>], members: &[usize]) -> bool {
    (0..KINDS.len()).all(|k| members.iter().map(|&m| loads[m][k]).sum::<f64>() * (1.0 + POOL_BUFFER) <= 1.0 + 1e-9)
}

/// Pairs in the same group that already share an account.
fn together(shared: &[Vec<bool>], parts: &[Vec<usize>]) -> usize {
    parts.iter().map(|p| p.iter().enumerate().map(|(i, &a)| p[i + 1..].iter().filter(|&&b| shared[a][b]).count()).sum::<usize>()).sum()
}

/// The computers (indices into `loads`, each fitting one plan alone) in the fewest groups that each
/// fit one plan, keeping most of those that share an account together on a tie.
fn pack(loads: &[Vec<f64>], shared: &[Vec<bool>]) -> Vec<Vec<usize>> {
    if loads.len() <= SEARCH_MAX {
        let mut best = None;
        search(loads, shared, 0, &mut vec![], &mut best);
        best.map(|(_, _, parts)| parts).unwrap_or_default()
    } else {
        first_fit(loads, shared)
    }
}

type Best = Option<(usize, usize, Vec<Vec<usize>>)>;

/// Tries every grouping of the computers from `i` on, given `parts` so far, keeping the best (fewest
/// groups, then most computers sharing an account together). A group that does not fit is never
/// grown, and a grouping with more groups than the best is not finished.
fn search(loads: &[Vec<f64>], shared: &[Vec<bool>], i: usize, parts: &mut Vec<Vec<usize>>, best: &mut Best) {
    if best.as_ref().is_some_and(|(groups, _, _)| parts.len() > *groups) { return; }
    if i == loads.len() {
        let kept = together(shared, parts);
        if best.as_ref().is_none_or(|(groups, most, _)| parts.len() < *groups || parts.len() == *groups && kept > *most) {
            *best = Some((parts.len(), kept, parts.clone()));
        }
        return;
    }
    for g in 0..parts.len() {
        parts[g].push(i);
        if fits(loads, &parts[g]) { search(loads, shared, i + 1, parts, best); }
        parts[g].pop();
    }
    parts.push(vec![i]);
    search(loads, shared, i + 1, parts, best);
    parts.pop();
}

/// Beyond `SEARCH_MAX`: the busiest computer first (in the weekly limit where it uses most) into
/// the first group it fits, one with a computer it shares an account with first, else a new group.
fn first_fit(loads: &[Vec<f64>], shared: &[Vec<bool>]) -> Vec<Vec<usize>> {
    let most = |i: usize| loads[i].iter().copied().fold(0.0, f64::max);
    let mut order: Vec<usize> = (0..loads.len()).collect();
    order.sort_by(|&a, &b| most(b).total_cmp(&most(a)).then(a.cmp(&b)));
    let mut parts: Vec<Vec<usize>> = vec![];
    for i in order {
        let fitting: Vec<usize> = (0..parts.len()).filter(|&g| fits(loads, &[parts[g].as_slice(), &[i]].concat())).collect();
        let to = fitting.iter().copied().find(|&g| parts[g].iter().any(|&m| shared[i][m])).or(fitting.first().copied());
        match to {
            Some(g) => parts[g].push(i),
            None => parts.push(vec![i]),
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageWindow;
    use crate::presence::{Peer, Seat};
    use crate::test_support::TempDir;

    fn at(s: &str) -> DateTime<Utc> { DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc) }
    fn days(n: i64) -> Duration { Duration::days(n) }
    fn hours(n: i64) -> Duration { Duration::hours(n) }

    /// A synthetic account: Claude plans by tier ("max" with "default_claude_max_20x", "pro"), Codex
    /// ones by the CLI's plan name ("pro" is Pro 200).
    fn account(provider: Provider, id: &str, plan: &str, tier: Option<&str>) -> AccountUsage {
        let mut a: AccountUsage = serde_json::from_value(serde_json::json!({
            "id": id, "provider": provider, "email": format!("{id}@example.invalid"), "alias": null, "plan": plan,
            "org": null, "active": false, "sources": [], "windows": [], "credits": null, "resets": [], "error": null,
            "stale": false, "fetched_at": null, "monitor_connected": true,
        })).unwrap();
        a.billing.tier = tier.map(String::from);
        a
    }

    fn max20(id: &str) -> AccountUsage { account(Provider::Claude, id, "max", Some("default_claude_max_20x")) }
    fn max5(id: &str) -> AccountUsage { account(Provider::Claude, id, "max", Some("default_claude_max_5x")) }

    fn weekly_window(acc: &AccountUsage, used: f64, resets: DateTime<Utc>) -> UsageWindow {
        let seconds = (acc.provider == Provider::Codex).then_some(WEEK_SECONDS);
        UsageWindow { kind: "weekly".into(), window_seconds: seconds, used_percent: used, resets_at: Some(resets) }
    }

    /// `acc` read at `now` with its weekly window at `used` percent, resetting at `resets`, in use
    /// here or not.
    fn read(acc: &AccountUsage, used: f64, resets: DateTime<Utc>, active: bool, now: DateTime<Utc>) -> AccountUsage {
        AccountUsage { active, fetched_at: Some(now), windows: vec![weekly_window(acc, used, resets)], ..acc.clone() }
    }

    fn refresh(ledger: &mut Ledger, accounts: Vec<AccountUsage>, now: DateTime<Utc>) {
        observe(ledger, &Snapshot { accounts, updated_at: Some(now) }, &[], &Providers::default(), now);
    }

    fn load_of(ledger: &Ledger, provider: Provider, now: DateTime<Utc>) -> ProviderLoad {
        summary(ledger, &Providers::default(), now).providers[provider.key()].clone()
    }

    /// The weekly limit's busiest 7 days and the 8 days to today, in plans.
    fn weekly(l: &ProviderLoad) -> (Option<f64>, Option<f64>) {
        (l.busiest.get("weekly").copied(), l.live.get("weekly").copied())
    }

    /// One refresh's readings: (account, used percent, in use here).
    type Readings<'a> = &'a [(&'a AccountUsage, f64, bool)];

    /// Refreshes at noon UTC on each day `from` on, one a day, each with its readings of windows
    /// resetting at `resets`.
    fn daily(ledger: &mut Ledger, resets: DateTime<Utc>, from: DateTime<Utc>, each_day: &[Readings]) {
        for (i, readings) in each_day.iter().enumerate() {
            let now = from + days(i as i64);
            refresh(ledger, readings.iter().map(|(a, used, active)| read(a, *used, resets, *active, now)).collect(), now);
        }
    }

    #[test]
    fn a_computer_is_charged_every_window_it_had_in_use_whole() {
        let x = max20("x");
        let (end, from) = (at("2026-11-09T00:00:00Z"), at("2026-11-02T12:00:00Z"));
        // DESK-1 is on x all week, alone: its charge is exactly x's week.
        let mut desk = Ledger::default();
        daily(&mut desk, end, from, &[&[(&x, 10.0, true)], &[(&x, 10.0, true)], &[(&x, 20.0, true)], &[(&x, 30.0, true)], &[(&x, 40.0, true)], &[(&x, 50.0, true)], &[(&x, 55.0, true)]]);
        assert_eq!(weekly(&load_of(&desk, Provider::Claude, end + hours(2))), (Some(0.55), Some(0.55)));
        // LAPTOP-2 had x in use on Thursday only: it is charged the whole window as well, the rises
        // before and after it was there included. Two computers on one window, both whole.
        let mut laptop = Ledger::default();
        daily(&mut laptop, end, from, &[&[(&x, 10.0, false)], &[(&x, 10.0, false)], &[(&x, 20.0, false)], &[(&x, 30.0, true)], &[(&x, 40.0, false)], &[(&x, 50.0, false)], &[(&x, 55.0, false)]]);
        assert_eq!(weekly(&load_of(&laptop, Provider::Claude, end + hours(2))).0, Some(0.55));
        // A computer that never had x in use is not charged for it.
        let mut other = Ledger::default();
        daily(&mut other, end, from, &[&[(&x, 10.0, false)], &[(&x, 55.0, false)]]);
        assert!(other.windows.is_empty());
        assert_eq!(weekly(&load_of(&other, Provider::Claude, end + hours(2))), (None, None));
        // The ledger names the account by a code only.
        let text = String::from_utf8(stored(&desk).unwrap()).unwrap();
        assert!(!text.contains("example.invalid") && !text.contains("\"x\""), "{text}");
    }

    #[test]
    fn a_burst_across_two_windows_counts_whole() {
        // x stays at 10% until Saturday, then 10 -> 60% over the weekend; its next week starts on
        // Monday and goes 0 -> 50% by Tuesday: a plan's worth in four days, which no single window shows.
        let x = max20("x");
        let mut l = Ledger::default();
        daily(&mut l, at("2026-11-09T00:00:00Z"), at("2026-11-02T12:00:00Z"),
            &[&[(&x, 10.0, true)], &[(&x, 10.0, true)], &[(&x, 10.0, true)], &[(&x, 10.0, true)], &[(&x, 10.0, true)], &[(&x, 35.0, true)], &[(&x, 60.0, true)]]);
        daily(&mut l, at("2026-11-16T00:00:00Z"), at("2026-11-09T12:00:00Z"), &[&[(&x, 20.0, true)], &[(&x, 50.0, true)]]);
        assert_eq!(weekly(&load_of(&l, Provider::Claude, at("2026-11-10T21:00:00Z"))), (Some(1.0), Some(1.0)));
        // Accounts used in turn add up the same way: y (Max 5x, a quarter of a Max 20x) after x.
        let y = max5("y");
        let mut l = Ledger::default();
        daily(&mut l, at("2026-11-09T00:00:00Z"), at("2026-11-02T12:00:00Z"), &[&[(&x, 0.0, true)], &[(&x, 0.0, true)], &[(&x, 0.0, true)], &[(&x, 0.0, true)], &[(&x, 30.0, true)], &[(&x, 50.0, false)]]);
        daily(&mut l, at("2026-11-13T00:00:00Z"), at("2026-11-07T12:00:00Z"), &[&[(&y, 10.0, true)], &[(&y, 30.0, true)]]);
        assert_eq!(weekly(&load_of(&l, Provider::Claude, at("2026-11-09T13:00:00Z"))).0, Some(0.58), "0.5 + 0.3 × 0.25, rounded up");
    }

    #[test]
    fn one_accounts_steady_weeks_are_overstated_by_a_day_at_most() {
        let x = max20("x");
        let mut l = Ledger::default();
        let step = 60.0 / 7.0;
        let week = |n: usize| -> Vec<f64> { (1..=n).map(|d| d as f64 * step).collect() };
        for (w, resets) in [at("2026-11-09T00:00:00Z"), at("2026-11-16T00:00:00Z"), at("2026-11-23T00:00:00Z")].into_iter().enumerate() {
            let used = week(7);
            let readings: Vec<[(&AccountUsage, f64, bool); 1]> = used.iter().map(|u| [(&x, *u, true)]).collect();
            let each: Vec<Readings> = readings.iter().map(|r| r.as_slice()).collect();
            daily(&mut l, resets, at("2026-11-02T23:00:00Z") + days(7 * w as i64), &each);
        }
        let busiest = weekly(&load_of(&l, Provider::Claude, at("2026-11-23T01:00:00Z"))).0.unwrap();
        assert!((0.6..=0.6 * 8.0 / 7.0 + 0.011).contains(&busiest), "{busiest}");
    }

    #[test]
    fn an_account_used_and_left_between_two_refreshes_is_charged() {
        let (x, y, end) = (max20("x"), max20("y"), at("2026-11-14T00:00:00Z"));
        let mut l = Ledger::default();
        let first = at("2026-11-08T12:00:00Z");
        refresh(&mut l, vec![read(&x, 10.0, end, true, first), read(&y, 20.0, end, false, first)], first);
        // A switch to y and back to x, between refreshes.
        mark(&mut l, &[(Provider::Claude, "y".into())], first + Duration::minutes(2));
        mark(&mut l, &[(Provider::Claude, "y".into())], first + Duration::minutes(4));
        let next = first + Duration::minutes(5);
        refresh(&mut l, vec![read(&x, 10.0, end, true, next), read(&y, 45.0, end, false, next)], next);
        assert_eq!(weekly(&load_of(&l, Provider::Claude, next)).0, Some(0.55), "x's 10% and all of y's 45%");
        // A mark from before a window began is not in it, and once no window can hold it, it goes.
        let mut l = Ledger::default();
        mark(&mut l, &[(Provider::Claude, "y".into())], first - days(8));
        refresh(&mut l, vec![read(&y, 20.0, end, false, first)], first);
        assert!(l.windows.is_empty() && l.used.is_empty());
    }

    #[test]
    fn the_week_in_progress_counts_at_its_pace_after_three_days() {
        let (x, end) = (max20("x"), at("2026-11-30T00:00:00Z"));
        let mut l = Ledger::default();
        // Two days in, 20% used: that use, not its pace.
        let now = end - days(5);
        refresh(&mut l, vec![read(&x, 20.0, end, true, now)], now);
        assert_eq!(weekly(&load_of(&l, Provider::Claude, now)), (Some(0.2), Some(0.2)));
        // Four days in, 40%: on pace for 70%.
        let now = end - days(3);
        refresh(&mut l, vec![read(&x, 40.0, end, true, now)], now);
        assert_eq!(weekly(&load_of(&l, Provider::Claude, now)), (Some(0.7), Some(0.7)));
        // Once it has reset, the week counts at what it reached; nothing rose in the last 8 days
        // once they are past it.
        assert_eq!(weekly(&load_of(&l, Provider::Claude, end + hours(1))), (Some(0.4), Some(0.4)));
        assert_eq!(weekly(&load_of(&l, Provider::Claude, end + days(9))), (Some(0.4), Some(0.0)));
    }

    #[test]
    fn pro_200_weeks_begun_before_the_allowance_cut_count_double() {
        let pro = account(Provider::Codex, "c", "pro", None);
        let mut l = Ledger::default();
        // Began 2026-10-26, before the cut (2026-10-30): 30% of the old allowance is 60% of the new.
        daily(&mut l, at("2026-11-02T00:00:00Z"), at("2026-10-31T12:00:00Z"), &[&[(&pro, 30.0, true)]]);
        assert_eq!(weekly(&load_of(&l, Provider::Codex, at("2026-11-02T01:00:00Z"))).0, Some(0.6));
        let mut l = Ledger::default();
        daily(&mut l, at("2026-11-09T00:00:00Z"), at("2026-11-07T12:00:00Z"), &[&[(&pro, 30.0, true)]]);
        assert_eq!(weekly(&load_of(&l, Provider::Codex, at("2026-11-09T01:00:00Z"))).0, Some(0.3));
        // Pro 500 counts two and a half plans; a free plan counts nothing and says so.
        let mut l = Ledger::default();
        let (big, free) = (account(Provider::Codex, "m", "promax", None), account(Provider::Codex, "f", "free", None));
        daily(&mut l, at("2026-11-09T00:00:00Z"), at("2026-11-07T12:00:00Z"), &[&[(&big, 10.0, true), (&free, 90.0, true)]]);
        let codex = load_of(&l, Provider::Codex, at("2026-11-09T01:00:00Z"));
        assert_eq!((weekly(&codex).0, codex.uncounted), (Some(0.25), true));
    }

    #[test]
    fn resets_too_far_ahead_are_no_week_and_other_computers_readings_only_raise() {
        let (x, now) = (max20("x"), at("2026-11-23T10:00:00Z"));
        let mut l = Ledger::default();
        refresh(&mut l, vec![read(&x, 30.0, now + days(7) + hours(2), true, now)], now);
        assert!(l.windows.is_empty(), "a reset more than a week and an hour ahead");
        // A window recorded under a clock that was wrong goes once it reads as that far ahead.
        l.windows.push(Touched { provider: Provider::Claude, account: account_code(Provider::Claude, "x"), kind: "weekly".into(),
            resets_at: now + days(20), read_at: now, weight: Some(1.0), pro_200: false, daily: vec![30.0] });
        refresh(&mut l, vec![], now);
        assert!(l.windows.is_empty());
        // Another computer's reading of y, which was never in use here, touches nothing; of x, in
        // use here, it raises x's window.
        let (y, end) = (max20("y"), now + days(3));
        let shared = |a: &AccountUsage, used: f64| AccountUsage { shared: Some(crate::model::SharedReading { machine: "LAPTOP-2".into(), taken_at: now,
            windows: vec![weekly_window(a, used, end)] }), stale: true, ..a.clone() };
        refresh(&mut l, vec![read(&x, 20.0, end, true, now - hours(1)), shared(&y, 50.0)], now);
        assert_eq!(l.windows.len(), 1);
        let later = now + hours(1);
        refresh(&mut l, vec![AccountUsage { active: true, ..shared(&x, 35.0) }], later);
        assert_eq!(l.windows[0].peak(), 35.0);
    }

    /// A ledger whose provider refreshed on `per_week[i]` days of the week `i + 1` weeks before now's.
    fn refreshed(per_week: &[usize], now: DateTime<Utc>) -> (u8, bool) {
        let this_week = monday(now.date_naive());
        let days: BTreeSet<NaiveDate> = per_week.iter().enumerate()
            .flat_map(|(i, n)| (0..*n as i64).map(move |d| this_week - Duration::weeks(i as i64 + 1) + Duration::days(d))).collect();
        evidence(Some(&days), &[], now)
    }

    #[test]
    fn a_computer_is_measured_after_four_of_eight_weeks_the_latest_included() {
        let now = at("2026-11-25T12:00:00Z");
        assert_eq!(refreshed(&[3, 3, 3, 3], now), (4, true));
        assert_eq!(refreshed(&[7, 0, 0, 0, 0, 7, 7, 7], now), (4, true), "any four of the eight");
        assert_eq!(refreshed(&[3, 3, 3], now), (3, false), "three weeks are not enough");
        assert_eq!(refreshed(&[2, 3, 3, 3, 3], now), (4, false), "the latest week watched on two days only");
        assert_eq!(refreshed(&[0, 7, 7, 7, 7, 7], now), (5, false), "the latest week not watched");
        assert_eq!(refreshed(&[3, 3, 3, 0, 0, 0, 0, 0, 7], now), (3, false), "a ninth week back does not count");
        assert_eq!(evidence(None, &[], now), (0, false));
        // Days come from refreshes, one per UTC day however many, for each provider on.
        let mut l = Ledger::default();
        for d in 0..21 {
            for h in [1, 9, 17] { refresh(&mut l, vec![], at("2026-11-02T00:00:00Z") + days(d) + hours(h)); }
        }
        let codex_off = Providers { claude: true, codex: false };
        observe(&mut l, &Snapshot::default(), &[], &codex_off, at("2026-11-23T08:00:00Z"));
        assert_eq!((l.days["claude"].len(), l.days["codex"].len()), (22, 21));
        let load = summary(&l, &codex_off, at("2026-11-23T08:00:00Z"));
        assert_eq!(load.providers.keys().collect::<Vec<_>>(), ["claude"], "only providers on are published");
        assert_eq!((load.providers["claude"].weeks, load.providers["claude"].measured), (3, false));
    }

    #[test]
    fn a_week_whose_windows_end_went_unread_is_not_measured() {
        let x = max20("x");
        let now = at("2026-11-25T12:00:00Z");
        // Refreshed every day of the latest four weeks; x's window ending on Sunday 2026-11-22 was
        // last read 13 hours before its reset.
        let mut l = Ledger::default();
        for d in 1..=28 {
            let day = at("2026-10-26T06:00:00Z") + days(d - 1);
            refresh(&mut l, vec![], day);
        }
        let end = at("2026-11-22T20:00:00Z");
        refresh(&mut l, vec![read(&x, 40.0, end, true, end - hours(13))], end - hours(13));
        let mine: Vec<&Touched> = l.windows.iter().collect();
        assert_eq!(evidence(l.days.get("claude"), &mine, now), (3, false));
        // Read 11 hours before it: that week is measured.
        refresh(&mut l, vec![read(&x, 41.0, end, true, end - hours(11))], end - hours(11));
        let mine: Vec<&Touched> = l.windows.iter().collect();
        assert_eq!(evidence(l.days.get("claude"), &mine, now), (4, true));
    }

    #[test]
    fn use_that_cannot_be_counted_marks_the_provider_uncounted() {
        let (x, end) = (max20("x"), at("2026-11-26T00:00:00Z"));
        let now = at("2026-11-23T10:00:00Z");
        let uncounted = |l: &Ledger, when: DateTime<Utc>| load_of(l, Provider::Claude, when).uncounted;
        // The desktop app here signed in to an account SlopBar does not have.
        let mut l = Ledger::default();
        let drift = crate::model::SeatDrift { app: "Claude Desktop".into(), cli: Some("x".into()), desktop: None };
        refresh(&mut l, vec![AccountUsage { seats: Some(drift), ..read(&x, 10.0, end, true, now) }], now);
        assert!(uncounted(&l, now));
        // An account in use with limits of another length.
        let mut l = Ledger::default();
        let mut odd = read(&account(Provider::Codex, "c", "pro", None), 10.0, end, true, now);
        odd.windows.push(UsageWindow { kind: "window".into(), window_seconds: Some(3 * 86400), used_percent: 40.0, resets_at: Some(end) });
        refresh(&mut l, vec![odd], now);
        assert!(load_of(&l, Provider::Codex, now).uncounted);
        // An account in use whose readings fail all day: known once the day is over.
        let mut l = Ledger::default();
        let failing = AccountUsage { stale: true, error: Some("http_500".into()), ..read(&x, 10.0, end, true, now - days(1)) };
        refresh(&mut l, vec![failing.clone()], now);
        refresh(&mut l, vec![failing.clone()], now + hours(6));
        assert!(!uncounted(&l, now + hours(6)), "the day is not over");
        refresh(&mut l, vec![read(&x, 12.0, end, true, now + days(1))], now + days(1));
        assert!(uncounted(&l, now + days(1)));
        assert_eq!(l.gaps["claude"].iter().collect::<Vec<_>>(), [&now.date_naive()]);
        // One good reading that day is enough.
        let mut l = Ledger::default();
        refresh(&mut l, vec![failing.clone()], now);
        refresh(&mut l, vec![read(&x, 11.0, end, true, now + hours(1))], now + hours(1));
        refresh(&mut l, vec![read(&x, 12.0, end, true, now + days(1))], now + days(1));
        assert!(!uncounted(&l, now + days(1)));
        // A gap counts while a span of the lookback reaches it.
        let mut l = Ledger::default();
        gap(&mut l, Provider::Claude, now.date_naive() - days(62));
        assert!(uncounted(&l, now));
        l.gaps.insert("claude".into(), BTreeSet::from([now.date_naive() - days(63)]));
        assert!(!uncounted(&l, now));
    }

    #[test]
    fn a_load_holds_totals_only_and_only_while_readings_are_shared() {
        let (x, end) = (max20("x"), at("2026-11-30T00:00:00Z"));
        let mut l = Ledger::default();
        refresh(&mut l, vec![read(&x, 42.0, end, true, end - days(2))], end - days(2));
        let now = end - days(2);
        let off = Settings { share_readings: false, ..Settings::default() };
        assert!(published(&l, &off, now).is_none());
        let record = crate::presence::own("11111111-1111-4111-8111-111111111111", "DESK-1", 300, &[], now).with_load(published(&l, &off, now));
        assert!(!serde_json::to_string(&record).unwrap().contains("load"), "no load while readings are not shared");
        let on = Settings { share_readings: true, ..Settings::default() };
        let load = published(&l, &on, now).unwrap();
        let json = serde_json::to_value(&load).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json["providers"]["claude"]["busiest"], serde_json::json!({ "weekly": 0.59 }), "42% five days in is on pace for 58.8%, rounded up");
        let text = json.to_string();
        assert!(!text.contains(&crate::presence::hash(Provider::Claude, "x")) && !text.contains(&account_code(Provider::Claude, "x")), "{text}");
        // What it writes, it reads back.
        assert_eq!(Load::from_value(&json), Some(load));
    }

    fn record_with(load: &str) -> String {
        format!(r#"{{"v":1,"machine_id":"22222222-2222-4222-8222-222222222222","machine":"LAPTOP-2","written_at":"2026-11-23T10:00:00Z","interval_s":300,
            "seats":[{{"p":"claude","h":"95ebf9717011b553004c8c1c6a6241f1","where":["Windows"]}}],"load":{load}}}"#)
    }

    #[test]
    fn a_load_is_read_leniently_and_never_sinks_its_record() {
        let deep = format!("{}1{}", "[".repeat(300), "]".repeat(300));
        for bad in [r#""junk""#.to_string(), "null".into(), "[1,2]".into(), r#"{"v":2,"at":"2026-11-23T10:00:00Z","providers":{}}"#.into(),
            r#"{"v":1,"providers":{}}"#.into(), r#"{"v":"1","at":"2026-11-23T10:00:00Z"}"#.into(),
            // Read as raw JSON first: a number out of range and nesting too deep drop the load alone.
            r#"{"v":1,"at":"2026-11-23T10:00:00Z","providers":{"claude":{"busiest":{"weekly":1e400}}}}"#.into(),
            format!(r#"{{"v":1,"at":"2026-11-23T10:00:00Z","providers":{{"claude":{{"busiest":{{"weekly":0.1}},"x":{deep}}}}}}}"#)] {
            let p: crate::presence::Presence = serde_json::from_str(&record_with(&bad)).unwrap_or_else(|e| panic!("{e}: {bad}"));
            assert_eq!((p.seats.len(), p.load.is_none()), (1, true), "{bad}");
        }
        // A key twice within it: the last one, as JSON readers take it.
        let twice = r#"{"v":1,"at":"2026-11-23T10:00:00Z","providers":{"claude":{"busiest":{"weekly":0.1,"weekly":0.2},"uncounted":false}}}"#;
        let p: crate::presence::Presence = serde_json::from_str(&record_with(twice)).unwrap();
        assert_eq!(p.load.unwrap().providers["claude"].busiest["weekly"], 0.2);
        let odd = serde_json::json!({ "v": 1, "at": "2026-11-23T10:00:00Z", "future": true, "providers": {
            "claude": { "busiest": { "weekly": 0.123, "weekly_haiku": "x" }, "live": { "weekly": 1000 }, "weeks": 300, "measured": true, "uncounted": false },
            "codex": { "busiest": { "weekly": -3 }, "uncounted": false }, "gemini": { "busiest": { "weekly": 0.5 } } } });
        let load = Load::from_value(&odd).unwrap();
        assert_eq!(load.providers.keys().collect::<Vec<_>>(), ["claude"], "an unknown provider left out, a negative use is no use to rely on");
        let claude = &load.providers["claude"];
        assert_eq!(claude.busiest, BTreeMap::from([("weekly".to_string(), 0.13)]));
        assert_eq!((claude.live["weekly"], claude.weeks, claude.measured, claude.uncounted), (100.0, 8, true, false));
        // A known kind with a value that is no number, no busiest at all, or a live that is not an
        // object: that provider is unknown. Without `uncounted`, some use is taken as uncounted.
        for entry in [serde_json::json!({ "busiest": { "weekly": "0.1" }, "uncounted": false }), serde_json::json!({ "uncounted": false }),
            serde_json::json!({ "busiest": {}, "live": 3, "uncounted": false })] {
            let load = Load::from_value(&serde_json::json!({ "v": 1, "at": "2026-11-23T10:00:00Z", "providers": { "claude": entry } })).unwrap();
            assert!(load.providers.is_empty(), "{entry}");
        }
        let silent = Load::from_value(&serde_json::json!({ "v": 1, "at": "2026-11-23T10:00:00Z", "providers": { "claude": { "busiest": { "weekly": 0.1 } } } })).unwrap();
        assert!(silent.providers["claude"].uncounted && !silent.providers["claude"].measured);
        assert_eq!((plans(f64::NAN), plans(f64::INFINITY), plans(-0.0)), (100.0, 100.0, 0.0));
    }

    fn loaded(busiest: &[(&str, f64)], measured: bool) -> Load {
        let entry = ProviderLoad { busiest: busiest.iter().map(|(k, v)| (k.to_string(), *v)).collect(), live: BTreeMap::new(), weeks: if measured { 6 } else { 2 }, measured, uncounted: false };
        Load { v: LOAD_VERSION, at: at("2026-11-23T09:00:00Z"), providers: BTreeMap::from([("claude".to_string(), entry)]) }
    }

    fn peer(id: &str, machine: &str, seen: DateTime<Utc>, load: Option<Load>, seats: &[&str]) -> Peer {
        Peer { machine_id: id.into(), machine: machine.into(), seen_at: seen, live: true, load,
            seats: seats.iter().map(|s| Seat { p: Provider::Claude, h: crate::presence::hash(Provider::Claude, s), where_: vec![] }).collect() }
    }

    fn place() -> crate::sharing::Place { crate::sharing::Place { remote: Some("cloud".into()), ..Default::default() } }

    /// (machine, busiest percent, measured, needs own) of each computer, and the groups.
    type Shown = (Vec<(String, Option<f64>, bool, bool)>, Vec<(Vec<String>, f64)>);

    fn shown(own: &Load, peers: &Peers, accounts: &[AccountUsage], now: DateTime<Utc>) -> Shown {
        let advice = advise(own, "DESK-1", peers, accounts, &Settings::default(), now);
        let Some(a) = advice.into_iter().find(|a| a.provider == Provider::Claude) else { return (vec![], vec![]) };
        (a.computers.into_iter().map(|c| (c.machine, c.busiest_percent, c.measured, c.needs_own)).collect(),
         a.groups.into_iter().map(|g| (g.machines, g.combined_percent)).collect())
    }

    fn names(list: &[&str]) -> Vec<String> { list.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn a_missing_load_is_unknown_and_the_last_one_stands() {
        let now = at("2026-11-23T10:00:00Z");
        let mut peers = Peers::default();
        let quiet = |id: &str, machine: &str, seen: DateTime<Utc>| Peer { live: false, ..peer(id, machine, seen, None, &[]) };
        peers.note(&place(), &[peer("b", "LAPTOP-2", now - hours(1), Some(loaded(&[("weekly", 0.3)], true)), &["x"]), quiet("c", "OLD-PC", now)], true, now);
        let own = loaded(&[("weekly", 0.4)], true);
        let accounts = [max20("x")];
        let (computers, groups) = shown(&own, &peers, &accounts, now);
        assert_eq!(computers, [("DESK-1".to_string(), Some(40.0), true, false), ("LAPTOP-2".to_string(), Some(30.0), true, false), ("OLD-PC".to_string(), None, false, false)],
            "an older SlopBar's computer is listed, its use unknown, never zero");
        assert_eq!(groups, [(names(&["DESK-1", "LAPTOP-2"]), 70.0)]);
        // Use on a plan of unknown size is missing from a computer's numbers: listed, never grouped.
        let mut partial = loaded(&[("weekly", 0.3)], true);
        partial.providers.get_mut("claude").unwrap().uncounted = true;
        let mut with_partial = Peers::default();
        with_partial.note(&place(), &[peer("b", "LAPTOP-2", now - hours(1), Some(partial), &["x"])], true, now);
        let (computers, groups) = shown(&own, &with_partial, &accounts, now);
        assert_eq!((computers.len(), groups.len()), (2, 0));
        // LAPTOP-2 quits: its empty record carries no load, and its last one stands.
        peers.note(&place(), &[quiet("b", "LAPTOP-2", now)], true, now);
        assert_eq!(shown(&own, &peers, &accounts, now).0[1], ("LAPTOP-2".to_string(), Some(30.0), true, false));
        // A week after it was worked out, it no longer does, though its record is still listed.
        let later = at("2026-11-23T09:00:00Z") + Duration::seconds(IGNORE_AFTER + 1);
        peers.note(&place(), &[quiet("b", "LAPTOP-2", now)], true, later);
        assert_eq!(shown(&own, &peers, &accounts, later).0[1], ("LAPTOP-2".to_string(), None, false, false));
        // A load is dated no later than its record.
        let mut dated = Peers::default();
        let mut ahead = loaded(&[("weekly", 0.3)], true);
        ahead.at = now + days(30);
        dated.note(&place(), &[peer("b", "LAPTOP-2", now, Some(ahead), &[])], true, now);
        assert_eq!(dated.by_id["b"].load.as_ref().unwrap().at, now);
        // Running with accounts in use and no load (readings no longer shared there, an older
        // SlopBar): unknown from then on.
        let mut stopped = Peers::default();
        stopped.note(&place(), &[peer("b", "LAPTOP-2", now - hours(1), Some(loaded(&[("weekly", 0.3)], true)), &["x"])], true, now);
        stopped.note(&place(), &[peer("b", "LAPTOP-2", now, None, &["x"])], true, now);
        assert_eq!(shown(&own, &stopped, &accounts, now).0[1].1, None);
        // Sharing moved: nothing from the old place.
        let mut peers = Peers::default();
        peers.note(&place(), &[peer("b", "LAPTOP-2", now, Some(loaded(&[("weekly", 0.3)], true)), &[])], true, now);
        assert!(peers.at(&place()) && !peers.at(&crate::sharing::Place { folder: Some("D:\\Sync".into()), ..Default::default() }));
        peers.note(&crate::sharing::Place { folder: Some("D:\\Sync".into()), ..Default::default() }, &[], true, now);
        assert_eq!(shown(&own, &peers, &accounts, now).0.len(), 1);
    }

    #[test]
    fn remembered_accounts_in_use_are_bounded_and_go_with_their_record() {
        let now = at("2026-11-23T10:00:00Z");
        let mut peers = Peers::default();
        let many: Vec<String> = (0..100).map(|i| format!("acct-{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let mut long = peer("b", "LAPTOP-2", now, Some(loaded(&[("weekly", 0.3)], true)), &many);
        long.seats[0].h = "f".repeat(5000);
        peers.note(&place(), &[long], true, now);
        let seats = &peers.by_id["b"].seats;
        assert_eq!((seats.len(), seats[0].1.len()), (SEATS_MAX, SEAT_MAX));
        // A read that failed says nothing; a listing without the record drops them, not the load.
        peers.note(&place(), &[], false, now);
        assert_eq!(peers.by_id["b"].seats.len(), SEATS_MAX);
        peers.note(&place(), &[], true, now);
        assert!(peers.by_id["b"].seats.is_empty() && peers.by_id["b"].load.is_some());
    }

    #[test]
    fn a_load_from_a_clock_running_ahead_is_dropped() {
        let now = Utc::now();
        let record = |written: DateTime<Utc>| crate::presence::own("22222222-2222-4222-8222-222222222222", "LAPTOP-2", 300, &[], written)
            .with_load(Some(loaded(&[("weekly", 0.3)], true)));
        let within = crate::presence::peers([(record(now + Duration::minutes(4)), None)], "me", now);
        assert!(within[0].load.is_some(), "a few minutes is clock drift");
        let ahead = crate::presence::peers([(record(now + Duration::minutes(10)), Some(now))], "me", now);
        assert_eq!((ahead.len(), ahead[0].load.is_none()), (1, true), "the record stands, its load goes");
        // A record the next read misses comes back from memory without it too.
        let mut recall = crate::presence::Recall::default();
        recall.fill(&place(), Some(&[(record(now + Duration::minutes(10)), Some(now))]), "me", now);
        let missed = crate::presence::peers(recall.fill(&place(), Some(&[]), "me", now + Duration::minutes(1)), "me", now + Duration::minutes(1));
        assert_eq!((missed.len(), missed[0].load.is_none()), (1, true));
    }

    #[test]
    fn records_under_one_name_add_up_this_computers_own_included() {
        let now = at("2026-11-23T10:00:00Z");
        // Another Windows profile on this PC runs its own SlopBar under the same computer name.
        let mut peers = Peers::default();
        peers.note(&place(), &[peer("p2", "desk-1", now, Some(loaded(&[("weekly", 0.2)], true)), &[]),
            peer("z", "DESK\u{200B}-1", now, Some(loaded(&[("weekly", 0.1)], true)), &[]),
            // LAPTOP-2 reinstalled: a new id under the same name, its old record still listed.
            peer("b-old", "LAPTOP-2", now - days(3), Some(loaded(&[("weekly", 0.25)], true)), &[]),
            peer("b-new", "laptop-2\u{FEFF}", now, Some(loaded(&[("weekly", 0.1)], false)), &[])], true, now);
        let (computers, _) = shown(&loaded(&[("weekly", 0.1)], true), &peers, &[max20("x")], now);
        assert_eq!(computers, [("DESK-1".to_string(), Some(40.0), true, false), ("LAPTOP-2".to_string(), Some(35.0), false, false)],
            "added up; measured only if every one is");
        // One of them with use uncounted, or with no load: the computer is uncounted, never grouped.
        let mut peers = Peers::default();
        peers.note(&place(), &[peer("p2", "DESK-1", now, None, &["y"]), peer("b", "LAPTOP-2", now, Some(loaded(&[("weekly", 0.1)], true)), &[])], true, now);
        let advice = advise(&loaded(&[("weekly", 0.1)], true), "DESK-1", &peers, &[max20("x")], &Settings::default(), now);
        let here = &advice[0].computers[0];
        assert_eq!((here.busiest_percent, here.measured, here.uncounted), (Some(10.0), false, true));
        assert!(advice[0].groups.is_empty());
    }

    #[test]
    fn one_account_is_the_reference_plan_when_one_renews_else_the_largest_held() {
        let now = at("2026-11-23T10:00:00Z");
        let settings = Settings::default();
        let ending = |mut a: AccountUsage| { a.billing.cancels_at = Some(now + days(9)); a };
        let of = |accounts: &[AccountUsage], p: Provider| unit(p, accounts, &settings, now);
        assert_eq!(of(&[max5("a"), max20("b")], Provider::Claude), ("Max 20x".into(), 1.0));
        assert_eq!(of(&[max5("a"), ending(max20("b"))], Provider::Claude), ("Max 5x".into(), 0.25), "a Max 20x that ends is not one account");
        assert_eq!(of(&[account(Provider::Claude, "p", "pro", None), max5("a")], Provider::Claude), ("Max 5x".into(), 0.25));
        assert_eq!(of(&[ending(max5("a"))], Provider::Claude), ("Max 5x".into(), 0.25), "nothing renews: the largest held");
        assert_eq!(of(&[account(Provider::Claude, "f", "free", None)], Provider::Claude), ("Max 20x".into(), 1.0));
        let codex = |plan: &str| account(Provider::Codex, plan, plan, None);
        assert_eq!(of(&[codex("plus"), codex("promax")], Provider::Codex), ("Pro 500".into(), 2.5));
        assert_eq!(of(&[codex("pro"), codex("promax")], Provider::Codex), ("Pro 200".into(), 1.0));
        // Letting a plan lapse is not renewing it either.
        let mut lapsing = Settings::default();
        lapsing.subscriptions.insert(subscriptions::pref_key(Provider::Claude, "b"), crate::settings::Subscription { cancel_after: Some(now.date_naive() + days(5)), ..Default::default() });
        assert_eq!(unit(Provider::Claude, &[max5("a"), max20("b")], &lapsing, now), ("Max 5x".into(), 0.25));
        // Percentages count in that plan.
        let (computers, _) = shown(&loaded(&[("weekly", 0.1)], true), &Peers::default(), &[max5("a")], now);
        assert_eq!(computers[0].1, Some(40.0));
    }

    /// Another computer: (name, busiest per kind, measured, accounts in use).
    type Machine<'a> = (&'a str, &'a [(&'a str, f64)], bool, &'a [&'a str]);

    fn fleet(loads: &[Machine], now: DateTime<Utc>) -> Peers {
        let mut peers = Peers::default();
        let list: Vec<Peer> = loads.iter().map(|(name, busiest, measured, seats)| peer(name, name, now, Some(loaded(busiest, *measured)), seats)).collect();
        peers.note(&place(), &list, true, now);
        peers
    }

    #[test]
    fn computers_are_packed_into_the_fewest_plans_and_heavy_ones_need_their_own() {
        let now = at("2026-11-23T10:00:00Z");
        let peers = fleet(&[("A", &[("weekly", 0.3)], true, &[]), ("B", &[("weekly", 0.2)], true, &[]), ("C", &[("weekly", 0.6)], true, &[]),
            ("D", &[("weekly", 0.9)], true, &[]), ("E", &[("weekly", 0.05)], false, &[])], now);
        let (computers, groups) = shown(&loaded(&[("weekly", 0.5)], true), &peers, &[max20("x")], now);
        let needs: Vec<&str> = computers.iter().filter(|c| c.3).map(|c| c.0.as_str()).collect();
        assert_eq!(needs, ["D"], "0.9 plus 20% is more than a plan");
        // 1.6 plans in all: two plans, which only {DESK-1, A} and {B, C} make (with 20% spare each).
        assert_eq!(groups, [(names(&["DESK-1", "A"]), 80.0), (names(&["B", "C"]), 80.0)], "E is not measured yet: in no group");
    }

    #[test]
    fn a_group_fits_every_weekly_limit() {
        let now = at("2026-11-23T10:00:00Z");
        let peers = fleet(&[("A", &[("weekly", 0.3), ("weekly_opus", 0.5)], true, &[])], now);
        let (_, groups) = shown(&loaded(&[("weekly", 0.3), ("weekly_opus", 0.4)], true), &peers, &[max20("x")], now);
        assert!(groups.is_empty(), "0.6 weekly fits, 0.9 of the Opus limit does not");
        let peers = fleet(&[("A", &[("weekly", 0.3), ("weekly_opus", 0.3)], true, &[])], now);
        let (_, groups) = shown(&loaded(&[("weekly", 0.3), ("weekly_opus", 0.4)], true), &peers, &[max20("x")], now);
        assert_eq!(groups, [(names(&["DESK-1", "A"]), 70.0)]);
    }

    #[test]
    fn of_equally_few_plans_computers_sharing_an_account_stay_together() {
        let now = at("2026-11-23T10:00:00Z");
        // Any two of three fit one plan, never all three: two plans whichever pair shares. B and C
        // are on one account already.
        let peers = fleet(&[("B", &[("weekly", 0.4)], true, &["y"]), ("C", &[("weekly", 0.4)], true, &["y"])], now);
        let (_, groups) = shown(&loaded(&[("weekly", 0.4)], true), &peers, &[max20("x")], now);
        assert_eq!(groups, [(names(&["B", "C"]), 80.0)]);
        // With this computer on that account instead, it is DESK-1 and C.
        let peers = fleet(&[("B", &[("weekly", 0.4)], true, &["z"]), ("C", &[("weekly", 0.4)], true, &["x"])], now);
        let mut here = max20("x");
        here.active = true;
        let (_, groups) = shown(&loaded(&[("weekly", 0.4)], true), &peers, &[here], now);
        assert_eq!(groups, [(names(&["DESK-1", "C"]), 80.0)]);
    }

    #[test]
    fn many_computers_are_placed_busiest_first_and_still_fit() {
        let now = at("2026-11-23T10:00:00Z");
        let list: Vec<(String, [(&str, f64); 1])> = (0..14).map(|i| (format!("PC-{i:02}"), [("weekly", [0.5, 0.3, 0.2, 0.1][i % 4])])).collect();
        let machines: Vec<Machine> = list.iter().map(|(n, b)| (n.as_str(), b.as_slice(), true, &[][..])).collect();
        let (computers, groups) = shown(&loaded(&[("weekly", 0.1)], true), &fleet(&machines, now), &[max20("x")], now);
        assert_eq!(computers.len(), 15);
        let total: f64 = computers.iter().filter_map(|c| c.1).sum();
        for (members, combined) in &groups {
            assert!(combined * 1.2 <= 100.0 + 1e-6, "{members:?} at {combined}");
        }
        let plans = groups.len() + computers.len() - groups.iter().map(|(m, _)| m.len()).sum::<usize>();
        assert!(plans as f64 <= (total * 1.2 / 100.0).ceil() + 1.0, "{plans} plans for {total}%");
    }

    #[test]
    fn a_provider_shows_only_where_some_computer_used_it() {
        let now = at("2026-11-23T10:00:00Z");
        let idle = Load { v: LOAD_VERSION, at: now, providers: BTreeMap::from([("claude".to_string(), ProviderLoad::default()), ("codex".to_string(), ProviderLoad::default())]) };
        assert!(advise(&idle, "DESK-1", &Peers::default(), &[], &Settings::default(), now).is_empty());
        let mut used = idle.clone();
        used.providers.get_mut("codex").unwrap().busiest.insert("weekly".into(), 0.2);
        let advice = advise(&used, "DESK-1", &Peers::default(), &[], &Settings::default(), now);
        assert_eq!(advice.iter().map(|a| (a.provider, a.unit.as_str(), a.weeks_needed)).collect::<Vec<_>>(), [(Provider::Codex, "Pro 200", 4)]);
        let off = Settings { providers: Providers { claude: true, codex: false }, ..Settings::default() };
        assert!(advise(&used, "DESK-1", &Peers::default(), &[], &off, now).is_empty(), "a provider turned off is not shown");
    }

    /// `count` windows with every field at its longest, resetting `step` minutes apart.
    fn full(now: DateTime<Utc>, count: usize, step: i64) -> Ledger {
        let mut full = Ledger { version: VERSION, ..Default::default() };
        for i in 0..count {
            let resets_at = now + days(6) - Duration::minutes(i as i64 * step);
            full.windows.push(Touched { provider: Provider::Claude, account: account_code(Provider::Claude, &format!("account-{i}")), kind: "weekly_sonnet".into(),
                resets_at, read_at: resets_at - Duration::minutes(7), weight: Some(0.25), pro_200: true, daily: vec![33.33, 44.44, 55.55, 66.66, 77.77, 88.88, 99.99, 100.0, 100.0] });
        }
        for p in Provider::ALL {
            full.days.insert(p.key().into(), (0..KEEP_WEEKS * 7 + 3).map(|d| now.date_naive() - days(d)).collect());
            full.gaps.insert(p.key().into(), (0..KEEP_WEEKS * 7 + 3).map(|d| now.date_naive() - days(d)).collect());
        }
        full
    }

    #[test]
    fn the_ledger_round_trips_and_stays_within_its_size() {
        let dir = TempDir::new();
        let path = dir.0.join(FILE);
        let (x, end) = (max20("x"), at("2026-11-09T00:00:00Z"));
        let mut l = Ledger::default();
        daily(&mut l, end, at("2026-11-05T12:00:00Z"), &[&[(&x, 12.5, true)], &[(&x, 20.0, true)]]);
        mark(&mut l, &[(Provider::Claude, "y".into())], at("2026-11-06T13:00:00Z"));
        save_to(&path, &l).unwrap();
        let back = load_from(&path);
        assert_eq!((back.version, back.windows.clone(), back.days.clone(), back.used.clone(), back.today.clone()), (VERSION, l.windows.clone(), l.days.clone(), l.used.clone(), l.today.clone()));
        std::fs::write(&path, br#"{"version":1,"windows":[],"days":{}}"#).unwrap();
        assert!(load_from(&path).days.is_empty(), "another version starts over");
        std::fs::write(&path, vec![b' '; MAX_FILE as usize + 1]).unwrap();
        assert!(load_from(&path).days.is_empty(), "a file past the cap is not read");
        // The most a ledger holds, every field at its longest, fits the cap as it is.
        let now = at("2026-11-23T12:34:56Z");
        let mut most = full(now, MAX_WINDOWS, 271);
        prune(&mut most, now);
        assert!(most.windows.len() == MAX_WINDOWS && most.days["claude"].len() <= KEEP_WEEKS as usize * 7 + 1);
        let bytes = stored(&most).unwrap();
        assert!(bytes.len() as u64 <= MAX_FILE / 2, "{} bytes", bytes.len());
        assert_eq!(serde_json::from_slice::<Ledger>(&bytes).unwrap().windows.len(), most.windows.len(), "nothing dropped to fit");
        // Should it not fit, the oldest windows go until it does, and the computer reads as
        // uncounted while a span reaches them.
        let huge = full(now, 1800, 20);
        let bytes = stored(&huge).unwrap();
        let kept: Ledger = serde_json::from_slice(&bytes).unwrap();
        assert!(bytes.len() as u64 <= MAX_FILE && !kept.windows.is_empty() && kept.windows.len() < huge.windows.len());
        assert!(kept.trimmed.is_some() && load_of(&kept, Provider::Claude, now).uncounted);
    }

    #[test]
    fn windows_a_span_reaches_are_never_trimmed_for_count() {
        let now = at("2026-11-23T12:00:00Z");
        let mut l = Ledger { version: VERSION, ..Default::default() };
        // 400 windows within the lookback, and 20 from before it.
        for i in 0..420 {
            let resets_at = if i < 400 { now + days(3) - Duration::minutes(i as i64 * 20) } else { now - days(70) - hours(i as i64 - 400) };
            l.windows.push(Touched { provider: Provider::Claude, account: format!("{i:016x}"), kind: "weekly".into(), resets_at, read_at: now, weight: Some(1.0), pro_200: false, daily: vec![1.0] });
        }
        prune(&mut l, now);
        assert_eq!(l.windows.len(), 400, "the 20 before the lookback go, the rest stay beyond MAX_WINDOWS");
    }

    /// How app.rs feeds the ledger, which the tests above drive directly: every account in use
    /// between two refreshes is marked (a switch once it took, a lease taken, released or ended, and
    /// at a refresh's start what the last one found in use and what is leased, before ended leases
    /// go), a refresh observes after its bookkeeping and says when the ledger could not be saved,
    /// and the advice reads the other computers only where sharing is now.
    #[test]
    fn the_app_marks_every_account_in_use_between_refreshes() {
        use crate::source_guard::{find, function, has, lost, tokens};
        const TEST: &str = "machine_load::tests::the_app_marks_every_account_in_use_between_refreshes";
        let app = tokens(include_str!("app.rs"));
        let before = |code: &str, place: &str, first: &str, then: &str| {
            let a = find(code, first).unwrap_or_else(|| panic!("{}", lost(place, first, TEST)));
            let b = find(code, then).unwrap_or_else(|| panic!("{}", lost(place, then, TEST)));
            assert!(a < b, "in {place}, `{first}` comes before `{then}`");
        };
        let switch = function(&app, "app.rs", "async fn switch_to(", TEST);
        before(switch, "switch_to in app.rs", "result?;", "mark_in_use(&app.state::<AppState>(), &[left, vec![(provider, id.clone())]].concat(), chrono::Utc::now());");
        let lease = function(&app, "app.rs", "async fn lease_account(", TEST);
        before(lease, "lease_account in app.rs", ".take(provider, &id, purpose, minutes, pid, chrono::Utc::now())?;", "mark_in_use(&state, &[(provider, id.clone())], lease.created_at);");
        assert!(has(&app, ".release(&lease_id).ok_or(\"no_such_lease\")?; mark_in_use(&app.state::<AppState>(), &[(released.provider, released.id.clone())], chrono::Utc::now());"),
            "{}", lost("ReleaseLease in app.rs", "the mark of a released lease", TEST));
        let prune = function(&app, "app.rs", "async fn prune_leases(", TEST);
        before(prune, "prune_leases in app.rs", "mark_in_use(&state, &leased, chrono::Utc::now());", ".prune(chrono::Utc::now(), leases::process_alive)");
        let refresh = function(&app, "app.rs", "async fn refresh_locked(", TEST);
        before(refresh, "refresh_locked in app.rs", "mark_in_use(&state, &was, chrono::Utc::now());", "state.leases.lock().unwrap().prune(chrono::Utc::now(), leases::process_alive);");
        before(refresh, "refresh_locked in app.rs", "subscriptions::attach_room(&mut snapshot, &history, now);",
            "machine_load::observe(&mut ledger, &snapshot, &wanted, &settings.providers, chrono::Utc::now());");
        assert!(has(refresh, "if machine_load::save(&machine_ledger).is_err() { state.diagnostics.record(None, None, diagnostics::Event::Action { action: diagnostics::Action::MachineLoadSave, code: diagnostics::Code::Storage, elapsed_ms: 0 }); }"),
            "{}", lost("refresh_locked in app.rs", "the ledger's save failure", TEST));
        assert!(has(refresh, "state.machine_peers.lock().unwrap().note(&read_from, &every, result.read, now);"), "{}", lost("refresh_locked in app.rs", "Peers::note", TEST));
        let advice = function(&app, "app.rs", "fn machine_advice(", TEST);
        assert!(has(advice, "let peers = if peers.at(&sharing::Place::of(settings)) { &*peers } else { &none };"), "{}", lost("machine_advice in app.rs", "Peers::at", TEST));
    }

    #[test]
    fn idle_weeks_and_other_windows_are_not_touched() {
        let x = max20("x");
        let now = at("2026-11-23T10:00:00Z");
        let mut l = Ledger::default();
        let mut acc = read(&x, 0.0, now + days(7), true, now);
        acc.windows.push(UsageWindow { kind: "session".into(), window_seconds: Some(18000), used_percent: 80.0, resets_at: Some(now + hours(3)) });
        refresh(&mut l, vec![acc], now);
        assert!(l.windows.is_empty(), "a week not begun and a session window");
        // Pinned to a project here, or leased here: in use.
        let mut l = Ledger::default();
        observe(&mut l, &Snapshot { accounts: vec![read(&x, 5.0, now + days(4), false, now)], updated_at: None }, &[(Provider::Claude, "x".into())], &Providers::default(), now);
        assert_eq!(l.windows.len(), 1);
    }
}
