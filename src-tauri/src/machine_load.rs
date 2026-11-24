//! How much each of the user's computers uses, whatever account it is on, and which computers use
//! little enough to share one plan.
//!
//! SlopBar measures usage per account; a computer's use is worked out here by ceiling attribution,
//! safe by construction: a computer is charged the whole reading of every account weekly window it
//! had in use at any refresh during that window (the CLI's sign-in here, from any source, WSL
//! included; the desktop app's; an account a project pin points to; an account leased here). The
//! charge is the window's highest reading this computer saw, times its plan's weight
//! (`subscriptions::plan_weight`), so use from different tiers adds up in plans of the provider's
//! reference plan. Nothing is split between computers and no token counts are used: two computers
//! on one account are each charged all of it. That can only overstate a computer's use, never
//! understate it, and it is exact for a light computer that stays on its own account.
//!
//! Each computer keeps its `Ledger` (the windows it touched, the days it refreshed), sums it into
//! a `Load` (its busiest 7 days per provider and weekly window kind) and, while readings are shared,
//! publishes that in its presence record (`presence::Presence::load`): totals only, never an account
//! code or id. The other computers' loads are remembered (`Peers`), and `advise` groups the
//! computers measured long enough into the fewest plans that carry them with 20% to spare.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::accounts::Provider;
use crate::model::{AccountUsage, Snapshot};
use crate::settings::{Providers, Settings};
use crate::subscriptions::{self, IDLE_SLACK_SECONDS, KINDS, POOL_BUFFER, SAME_WINDOW_SECONDS, WEEK_SECONDS};

pub(crate) const VERSION: u8 = 1;
/// The `load.v` this SlopBar writes and reads; a load of any other is dropped.
const LOAD_VERSION: u32 = 1;
const FILE: &str = "machine-load.json";
/// The ledger file is read up to this size; `save` keeps it within.
const MAX_FILE: u64 = 64 * 1024;
/// Windows and refresh days are kept this many weeks.
const KEEP_WEEKS: i64 = 12;
/// At most this many windows are kept, the oldest dropped first: well inside `MAX_FILE`.
const MAX_WINDOWS: usize = 360;
/// The busiest 7 days are looked for among the windows ending in this many latest weeks, and the
/// evidence among as many latest completed weeks (`subscriptions::POOL_WEEKS`).
const LOOK_WEEKS: i64 = subscriptions::POOL_WEEKS as i64;
/// A UTC week (Monday to Sunday) is measured when SlopBar refreshed here on this many days of it.
const MEASURED_DAYS: usize = 3;
/// A computer is measured with this many measured weeks among the latest `LOOK_WEEKS` completed
/// ones, the latest of them included.
pub const WEEKS_NEEDED: u8 = 4;
/// Windows that overlap by no more than this are one after the other (resets jitter), not at once.
const OVERLAP_SECONDS: i64 = 3600;
/// Every grouping is tried up to this many computers (as `advisor` does for plans); beyond it, the
/// busiest is placed first in the first group it fits.
const SEARCH_MAX: usize = 10;
/// The longest machine name kept in memory (`presence` cuts names at 63 characters already).
const NAME_MAX: usize = 63;

// ---------------------------------------------------------------------------------------------
// The ledger: this computer's touched windows and refresh days

/// What this computer had in use and when it ran. Kept in `machine-load.json` in the app's folder,
/// never in backups. Accounts appear only as a code (`account_code`), never an id or email.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ledger {
    pub version: u8,
    /// Every account weekly window this computer had in use, with its highest reading.
    pub windows: Vec<Touched>,
    /// The UTC days SlopBar refreshed here with the provider on, by provider (`Provider::key`).
    pub days: BTreeMap<String, BTreeSet<NaiveDate>>,
}

/// One account weekly window this computer had in use. Short names keep a ledger of `MAX_WINDOWS`
/// windows well inside `MAX_FILE`.
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
    /// `SAME_WINDOW_SECONDS` of it are the same window).
    #[serde(rename = "r")]
    pub resets_at: DateTime<Utc>,
    /// The highest share of the window used that this computer read, in percent, from the moment it
    /// had the account in use on: in use here or not since, the whole window is charged.
    #[serde(rename = "u")]
    pub peak: f64,
    /// When the latest reading of it was taken.
    #[serde(rename = "t")]
    pub read_at: DateTime<Utc>,
    /// The plan's weight (`subscriptions::plan_weight`), the largest read; None while unknown.
    #[serde(rename = "w", default)]
    pub weight: Option<f64>,
    /// A Codex Pro 200 plan (`subscriptions::pro_200`): its windows begun before the allowance cut
    /// count against the new, smaller allowance.
    #[serde(rename = "x", default, skip_serializing_if = "std::ops::Not::not")]
    pub pro_200: bool,
}

/// An account as the ledger names it: the first 16 hex digits of its presence code
/// (`presence::hash`), enough to tell a user's accounts apart, never its id or email.
fn account_code(provider: Provider, id: &str) -> String {
    crate::presence::hash(provider, id)[..16].to_string()
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

/// The ledger as written: its oldest windows left out while it would not fit `MAX_FILE`, so the
/// file always reads back (`MAX_WINDOWS` keeps that from happening in practice).
fn stored(ledger: &Ledger) -> Result<Vec<u8>, String> {
    let mut kept = ledger.clone();
    kept.windows.sort_by_key(|t| t.resets_at);
    loop {
        let bytes = serde_json::to_vec(&kept).map_err(|_| "machine_load_storage")?;
        if bytes.len() as u64 <= MAX_FILE { return Ok(bytes); }
        if kept.windows.is_empty() {
            kept.days.clear();
        } else {
            let drop = (kept.windows.len() / 8).max(1);
            kept.windows.drain(..drop);
        }
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

/// Folds one refresh into the ledger. The day counts as refreshed for every provider on. Each
/// weekly window of an account in use here (`AccountUsage::active` or `desktop`, or in `wanted`:
/// pinned to a project or leased here) is touched; a touched window keeps its highest reading from
/// then on, whether or not the account is still in use here, since the whole window is charged.
/// Readings come from this computer's own read of the account, a stale one included (its numbers
/// are an earlier reading of the same window), and from another computer's newer one shown here
/// (`AccountUsage::shared`). A window not used yet (0% with its reset a full week ahead) has no
/// fixed reset and is not touched.
pub fn observe(ledger: &mut Ledger, snapshot: &Snapshot, wanted: &[(Provider, String)], providers: &Providers, now: DateTime<Utc>) {
    ledger.version = VERSION;
    for p in Provider::ALL.into_iter().filter(|p| enabled(providers, *p)) {
        ledger.days.entry(p.key().to_string()).or_default().insert(now.date_naive());
    }
    for acc in snapshot.accounts.iter().filter(|a| enabled(providers, a.provider)) {
        let in_use = acc.active || acc.desktop || wanted.iter().any(|(p, id)| *p == acc.provider && *id == acc.id);
        let account = account_code(acc.provider, &acc.id);
        let weight = subscriptions::plan_weight(acc);
        let pro_200 = subscriptions::pro_200(acc);
        let own = acc.fetched_at.unwrap_or(now).min(now);
        let readings = acc.windows.iter().map(|w| (w, own))
            .chain(acc.shared.iter().flat_map(|s| s.windows.iter().map(move |w| (w, s.taken_at.min(now)))));
        for (w, read_at) in readings {
            if !KINDS.contains(&w.kind.as_str()) || w.window_seconds.is_some_and(|s| s != WEEK_SECONDS) || !w.used_percent.is_finite() { continue; }
            let Some(resets_at) = w.resets_at.filter(|r| *r > now) else { continue };
            let used = w.used_percent.max(0.0);
            if used <= 0.0 && (resets_at - now).num_seconds() > WEEK_SECONDS - IDLE_SLACK_SECONDS { continue; }
            let same = ledger.windows.iter_mut().find(|t| t.provider == acc.provider && t.account == account && t.kind == w.kind
                && (t.resets_at - resets_at).num_seconds().abs() < SAME_WINDOW_SECONDS);
            match same {
                Some(t) => {
                    t.peak = t.peak.max(stored_percent(used));
                    t.read_at = t.read_at.max(whole_second(read_at));
                    t.weight = match (t.weight, weight) { (Some(a), Some(b)) => Some(a.max(b)), (a, b) => a.or(b) };
                    t.pro_200 |= pro_200;
                }
                None if in_use => ledger.windows.push(Touched {
                    provider: acc.provider, account: account.clone(), kind: w.kind.clone(), resets_at: whole_second(resets_at),
                    peak: stored_percent(used), read_at: whole_second(read_at), weight, pro_200,
                }),
                None => {}
            }
        }
    }
    prune(ledger, now);
}

/// A percent as the ledger keeps it: rounded up to a hundredth.
fn stored_percent(p: f64) -> f64 {
    (p * 100.0 - 1e-9).ceil() / 100.0 + 0.0
}

/// Drops windows and days older than `KEEP_WEEKS`, then the oldest windows beyond `MAX_WINDOWS`.
fn prune(ledger: &mut Ledger, now: DateTime<Utc>) {
    let since = now - Duration::weeks(KEEP_WEEKS);
    ledger.windows.retain(|t| t.resets_at > since);
    if ledger.windows.len() > MAX_WINDOWS {
        ledger.windows.sort_by_key(|t| t.resets_at);
        let extra = ledger.windows.len() - MAX_WINDOWS;
        ledger.windows.drain(..extra);
    }
    let first = since.date_naive();
    for days in ledger.days.values_mut() { days.retain(|d| *d >= first); }
    ledger.days.retain(|k, d| !d.is_empty() && Provider::ALL.iter().any(|p| p.key() == k));
}

// ---------------------------------------------------------------------------------------------
// A computer's load, as its presence record carries it

/// A computer's use as its presence record carries it (`presence::Presence::load`): per provider,
/// totals only, in plans of the provider's reference plan (`subscriptions::reference_plan`, weight
/// 1), rounded up to 0.01 plan; never an account code or id.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Load {
    pub v: u32,
    /// When it was worked out, by the writer's clock.
    pub at: DateTime<Utc>,
    /// By `Provider::key`, the providers on there.
    pub providers: BTreeMap<String, ProviderLoad>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProviderLoad {
    /// The busiest 7 days per weekly window kind, in plans (`busiest`); a kind it never touched is
    /// missing, which is no use of it.
    pub busiest: BTreeMap<String, f64>,
    /// The same for the windows still running: the week in progress.
    pub live: BTreeMap<String, f64>,
    /// Measured weeks among the latest `LOOK_WEEKS` completed ones (`evidence`).
    pub weeks: u8,
    pub measured: bool,
    /// It had an account in use whose plan has no known weight (a free plan, an unknown tier): that
    /// use is not in the numbers.
    pub uncounted: bool,
}

/// An amount of plans as a load carries it: rounded up to 0.01 plan, so rounding never makes a
/// computer look lighter, and within 0 to 100 plans.
fn plans(v: f64) -> f64 {
    if v.is_nan() { return 0.0; }
    ((v * 100.0 - 1e-9).ceil() / 100.0).clamp(0.0, 100.0) + 0.0
}

impl Load {
    /// A `load` as another computer's record carries it, read leniently: None unless it is an
    /// object of `LOAD_VERSION` with its time; within it, providers and kinds this SlopBar does not
    /// know are left out, numbers are taken as `plans`, and anything else malformed is left out or
    /// read as nothing.
    pub fn from_value(raw: &serde_json::Value) -> Option<Load> {
        if raw.get("v")?.as_u64()? != LOAD_VERSION as u64 { return None; }
        let at = raw.get("at")?.as_str()?.parse::<DateTime<Utc>>().ok()?;
        let mut providers = BTreeMap::new();
        for (key, entry) in raw.get("providers").and_then(|p| p.as_object()).into_iter().flatten() {
            if !Provider::ALL.iter().any(|p| p.key() == key) { continue; }
            let Some(entry) = entry.as_object() else { continue };
            let kinds = |field: &str| -> BTreeMap<String, f64> {
                entry.get(field).and_then(|m| m.as_object()).into_iter().flatten()
                    .filter(|(k, _)| KINDS.contains(&k.as_str()))
                    .filter_map(|(k, v)| Some((k.clone(), plans(v.as_f64()?))))
                    .collect()
            };
            providers.insert(key.clone(), ProviderLoad {
                busiest: kinds("busiest"),
                live: kinds("live"),
                weeks: entry.get("weeks").and_then(|w| w.as_u64()).map_or(0, |w| w.min(LOOK_WEEKS as u64) as u8),
                measured: entry.get("measured").and_then(|m| m.as_bool()).unwrap_or(false),
                uncounted: entry.get("uncounted").and_then(|m| m.as_bool()).unwrap_or(false),
            });
        }
        Some(Load { v: LOAD_VERSION, at, providers })
    }
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

/// The span a window covers: the week before its reset.
fn overlap_seconds(a: &Touched, b: &Touched) -> i64 {
    WEEK_SECONDS - (a.resets_at - b.resets_at).num_seconds().abs()
}

/// What a window charges this computer, in plans: its highest reading, or for a window still
/// running its use so far or its pace once three days have passed (`subscriptions::live_week`),
/// times its plan's weight. A Codex Pro 200 window begun before the allowance cut counts against
/// the new allowance (`subscriptions::PRO_200_NEW_ALLOWANCE`, about double), as `subscriptions::busiest`
/// counts it. A plan of unknown weight charges nothing (`ProviderLoad::uncounted`).
fn charge(t: &Touched, now: DateTime<Utc>) -> f64 {
    let Some(weight) = t.weight else { return 0.0 };
    let used = subscriptions::live_week(t.peak, t.resets_at, t.read_at, now).unwrap_or(t.peak);
    let began = t.resets_at - Duration::seconds(WEEK_SECONDS);
    let used = if t.pro_200 && began < subscriptions::pro_200_cut() { used / subscriptions::PRO_200_NEW_ALLOWANCE } else { used };
    used / 100.0 * weight
}

/// A provider's busiest 7 days per kind: over the windows ending in the latest `LOOK_WEEKS` weeks
/// and those still running, for each window the charges of every touched window overlapping it by
/// more than `OVERLAP_SECONDS` added up (itself included: accounts used in turn add up, one
/// account's weeks one after another do not), and the largest of those; `live` the same for the
/// windows still running.
fn provider_load(ledger: &Ledger, provider: Provider, now: DateTime<Utc>) -> ProviderLoad {
    let since = now - Duration::weeks(LOOK_WEEKS);
    let mine: Vec<&Touched> = ledger.windows.iter().filter(|t| t.provider == provider).collect();
    let mut out = ProviderLoad { uncounted: mine.iter().any(|t| t.resets_at > since && t.weight.is_none()), ..Default::default() };
    for kind in KINDS {
        let of_kind: Vec<(&Touched, f64)> = mine.iter().filter(|t| t.kind == kind).map(|t| (*t, charge(t, now))).collect();
        let around = |w: &Touched| of_kind.iter().filter(|(o, _)| overlap_seconds(w, o) > OVERLAP_SECONDS).map(|(_, c)| *c).sum::<f64>();
        let recent: Vec<&Touched> = of_kind.iter().map(|(t, _)| *t).filter(|t| t.resets_at > since).collect();
        if let Some(b) = recent.iter().map(|w| around(w)).reduce(f64::max) { out.busiest.insert(kind.to_string(), plans(b)); }
        if let Some(l) = recent.iter().filter(|w| w.resets_at > now).map(|w| around(w)).reduce(f64::max) { out.live.insert(kind.to_string(), plans(l)); }
    }
    (out.weeks, out.measured) = evidence(ledger.days.get(provider.key()), now);
    out
}

fn monday(d: NaiveDate) -> NaiveDate {
    d - Duration::days(d.weekday().num_days_from_monday() as i64)
}

/// The measured weeks among the latest `LOOK_WEEKS` completed UTC weeks (Monday to Sunday): those
/// SlopBar refreshed here on `MEASURED_DAYS` days or more. Measured with `WEEKS_NEEDED` of them, the
/// latest completed week one of them: use while SlopBar was not running here goes unseen, so a
/// computer is judged only on weeks it watched.
fn evidence(days: Option<&BTreeSet<NaiveDate>>, now: DateTime<Utc>) -> (u8, bool) {
    let this_week = monday(now.date_naive());
    let measured: Vec<bool> = (1..=LOOK_WEEKS).map(|i| {
        let start = this_week - Duration::weeks(i);
        days.map_or(0, |d| d.range(start..start + Duration::days(7)).count()) >= MEASURED_DAYS
    }).collect();
    let weeks = measured.iter().filter(|m| **m).count() as u8;
    (weeks, weeks >= WEEKS_NEEDED && measured[0])
}

// ---------------------------------------------------------------------------------------------
// The other computers' loads

/// The other computers' last loads, by machine id, in memory only. A record without a load (from
/// an older SlopBar, from one not sharing readings, or the empty record a computer leaves when it
/// quits) says nothing about its use: the last load stands, so a computer that is off is never read
/// as idle. Kept while the computer's newest record is still listed (`presence::IGNORE_AFTER`), and
/// forgotten when sharing moves to another place.
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
    /// Its accounts in use (provider, presence code) as of the record that carried `load`, so
    /// computers already sharing an account are kept together.
    seats: Vec<(Provider, String)>,
}

impl Peers {
    /// Takes what a read of `place` brought of every other computer (`presence::peers`, records the
    /// read missed included); `me` is this computer's name: a computer going by it is not another.
    pub fn note(&mut self, place: &crate::sharing::Place, peers: &[crate::presence::Peer], me: &str, now: DateTime<Utc>) {
        if !self.place.as_ref().is_some_and(|p| p.same_location(place)) {
            self.by_id.clear();
            self.place = Some(place.clone());
        }
        for p in peers.iter().filter(|p| !p.machine.eq_ignore_ascii_case(me)) {
            let e = self.by_id.entry(p.machine_id.clone())
                .or_insert_with(|| Remembered { machine: String::new(), seen_at: p.seen_at, load: None, seats: vec![] });
            e.machine = clean_name(&p.machine);
            e.seen_at = p.seen_at;
            if let Some(load) = &p.load {
                e.load = Some(load.clone());
                e.seats = p.seats.iter().map(|s| (s.p, s.h.clone())).collect();
            }
        }
        self.by_id.retain(|_, e| (now - e.seen_at).num_seconds() <= crate::presence::IGNORE_AFTER && !e.machine.is_empty());
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
    /// Its busiest 7 days (`ProviderLoad::busiest`) in the weekly limit where it comes to most, in
    /// percent of one `unit`; None when its use is unknown (no load from it for this provider).
    pub busiest_percent: Option<f64>,
    /// The same for the week in progress (0 with nothing running).
    pub live_percent: Option<f64>,
    pub weeks: u8,
    pub measured: bool,
    /// Measured, and its busiest 7 days plus 20% do not fit one `unit` in some weekly limit.
    pub needs_own: bool,
    /// Some of its use is on a plan of unknown weight and not in the numbers.
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

/// One computer as the advice weighs it.
struct Candidate<'a> {
    machine: String,
    here: bool,
    seen_at: DateTime<Utc>,
    load: Option<&'a ProviderLoad>,
    seats: Vec<String>,
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0 + 0.0
}

/// For every provider on with use on some computer: each computer with what it uses in percent of
/// one plan of `unit`, and the computers that could share one. `own` is this computer's load and
/// `here` its name; `accounts` are this computer's (for `unit` and the accounts in use here).
/// Groups are made of measured computers whose busiest 7 days plus 20% fit one plan in every weekly
/// limit, packed into the fewest plans so that each group's busiest 7 days added up (each
/// computer's own busiest, added whatever their dates: conservative) plus 20% fits one plan in
/// every weekly limit. Every grouping is tried up to `SEARCH_MAX` computers, then the busiest first
/// into the first group it fits; of equally few plans, the one keeping most computers that already
/// share an account together.
pub fn advise(own: &Load, here: &str, peers: &Peers, accounts: &[AccountUsage], settings: &Settings, now: DateTime<Utc>) -> Vec<MachineAdvice> {
    let here = clean_name(here);
    let mine = own_seats(accounts);
    // The newest record of each other name: a computer reinstalled under a new id is one computer.
    let mut others: Vec<&Remembered> = vec![];
    for e in peers.by_id.values().filter(|e| !e.machine.eq_ignore_ascii_case(&here)) {
        match others.iter_mut().find(|o| o.machine.eq_ignore_ascii_case(&e.machine)) {
            Some(o) => { if e.seen_at > o.seen_at { *o = e; } }
            None => others.push(e),
        }
    }
    others.sort_by(|a, b| a.machine.to_lowercase().cmp(&b.machine.to_lowercase()).then_with(|| a.machine.cmp(&b.machine)));
    let mut out = vec![];
    for provider in Provider::ALL.into_iter().filter(|p| enabled(&settings.providers, *p)) {
        let key = provider.key();
        let seats_of = |s: &[(Provider, String)]| s.iter().filter(|(p, _)| *p == provider).map(|(_, h)| h.clone()).collect::<Vec<_>>();
        let candidates: Vec<Candidate> = std::iter::once(Candidate { machine: here.clone(), here: true, seen_at: now, load: own.providers.get(key), seats: seats_of(&mine) })
            .chain(others.iter().map(|e| Candidate { machine: e.machine.clone(), here: false, seen_at: e.seen_at,
                load: e.load.as_ref().and_then(|l| l.providers.get(key)), seats: seats_of(&e.seats) }))
            .collect();
        // Shown only where some computer used the provider at all.
        if !candidates.iter().filter_map(|c| c.load).any(|l| l.busiest.values().any(|v| *v > 0.0)) { continue; }
        let (unit, weight) = unit(provider, accounts, settings, now);
        // Each computer's busiest 7 days per kind, in plans of `unit`.
        let shares = |l: &ProviderLoad, of: &dyn Fn(&ProviderLoad) -> &BTreeMap<String, f64>| -> Vec<f64> {
            KINDS.iter().map(|k| of(l).get(*k).copied().unwrap_or(0.0) / weight).collect()
        };
        let fits_one = |loads: &[f64]| loads.iter().all(|v| v * (1.0 + POOL_BUFFER) <= 1.0 + 1e-9);
        let mut computers = vec![];
        let mut members: Vec<(usize, Vec<f64>)> = vec![];
        for (i, c) in candidates.iter().enumerate() {
            let busiest = c.load.map(|l| shares(l, &|l| &l.busiest));
            let live = c.load.map(|l| shares(l, &|l| &l.live));
            let measured = c.load.is_some_and(|l| l.measured);
            let needs_own = measured && busiest.as_ref().is_some_and(|b| !fits_one(b));
            // Use on a plan of unknown size is missing from its numbers, so they may read low: such a
            // computer is listed, never grouped.
            let uncounted = c.load.is_some_and(|l| l.uncounted);
            if measured && !needs_own && !uncounted { members.push((i, busiest.clone().unwrap_or_default())); }
            let most = |v: &Vec<f64>| round2(v.iter().copied().fold(0.0, f64::max) * 100.0);
            computers.push(Computer {
                machine: c.machine.clone(),
                here: c.here,
                busiest_percent: busiest.as_ref().map(most),
                live_percent: live.as_ref().map(most),
                weeks: c.load.map_or(0, |l| l.weeks),
                measured,
                needs_own,
                uncounted,
                seen_at: c.seen_at,
            });
        }
        let loads: Vec<Vec<f64>> = members.iter().map(|(_, l)| l.clone()).collect();
        let shared: Vec<Vec<bool>> = members.iter().map(|(a, _)| members.iter()
            .map(|(b, _)| a != b && candidates[*a].seats.iter().any(|h| candidates[*b].seats.contains(h))).collect()).collect();
        let mut groups: Vec<MachineGroup> = pack(&loads, &shared).into_iter().filter(|g| g.len() > 1).map(|mut g| {
            g.sort_unstable();
            let combined = (0..KINDS.len()).map(|k| g.iter().map(|&m| loads[m][k]).sum::<f64>()).fold(0.0, f64::max);
            MachineGroup { machines: g.iter().map(|&m| candidates[members[m].0].machine.clone()).collect(), combined_percent: round2(combined * 100.0) }
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

    /// `acc` read at `now` with its weekly window at `used` percent, resetting at `resets`, in use
    /// here or not.
    fn read(acc: &AccountUsage, used: f64, resets: DateTime<Utc>, active: bool, now: DateTime<Utc>) -> AccountUsage {
        let seconds = (acc.provider == Provider::Codex).then_some(WEEK_SECONDS);
        AccountUsage { active, fetched_at: Some(now), windows: vec![UsageWindow { kind: "weekly".into(), window_seconds: seconds, used_percent: used, resets_at: Some(resets) }], ..acc.clone() }
    }

    fn refresh(ledger: &mut Ledger, accounts: Vec<AccountUsage>, now: DateTime<Utc>) {
        observe(ledger, &Snapshot { accounts, updated_at: Some(now) }, &[], &Providers::default(), now);
    }

    fn busiest(ledger: &Ledger, provider: Provider, now: DateTime<Utc>) -> ProviderLoad {
        summary(ledger, &Providers::default(), now).providers[provider.key()].clone()
    }

    fn weekly(l: &ProviderLoad) -> (Option<f64>, Option<f64>) {
        (l.busiest.get("weekly").copied(), l.live.get("weekly").copied())
    }

    /// One refresh's readings: (account, used percent, in use here).
    type Readings<'a> = &'a [(&'a AccountUsage, f64, bool)];

    /// Refreshes `hours_before` the window's reset, each with its readings.
    fn week_of(ledger: &mut Ledger, resets: DateTime<Utc>, refreshes: &[(i64, Readings)]) {
        for (hours_before, readings) in refreshes {
            let now = resets - hours(*hours_before);
            refresh(ledger, readings.iter().map(|(a, used, active)| read(a, *used, resets, *active, now)).collect(), now);
        }
    }

    #[test]
    fn a_computer_is_charged_every_window_it_had_in_use_whole() {
        let (x, end) = (max20("x"), at("2026-11-09T00:00:00Z"));
        // DESK-1 is on x all week, alone: its charge is exactly x's week.
        let mut desk = Ledger::default();
        week_of(&mut desk, end, &[(150, &[(&x, 10.0, true)]), (80, &[(&x, 30.0, true)]), (20, &[(&x, 55.0, true)])]);
        assert_eq!(weekly(&busiest(&desk, Provider::Claude, end + hours(2))), (Some(0.55), None));
        // LAPTOP-2 had x in use at one refresh only, before switching away: it is charged the whole
        // window as well, readings after it left included. Two computers on one window, both whole.
        let mut laptop = Ledger::default();
        week_of(&mut laptop, end, &[(150, &[(&x, 10.0, false)]), (80, &[(&x, 30.0, true)]), (20, &[(&x, 55.0, false)])]);
        assert_eq!(weekly(&busiest(&laptop, Provider::Claude, end + hours(2))), (Some(0.55), None));
        // A computer that never had x in use is not charged for it.
        let mut other = Ledger::default();
        week_of(&mut other, end, &[(150, &[(&x, 10.0, false)]), (20, &[(&x, 55.0, false)])]);
        assert!(other.windows.is_empty());
        assert_eq!(weekly(&busiest(&other, Provider::Claude, end + hours(2))), (None, None));
        // The ledger names the account by a code only.
        let text = String::from_utf8(stored(&desk).unwrap()).unwrap();
        assert!(!text.contains("example.invalid") && !text.contains("\"x\""), "{text}");
    }

    #[test]
    fn accounts_used_in_turn_add_up_and_one_accounts_weeks_do_not() {
        let (x, y) = (max20("x"), max5("y"));
        let (x_end, y_end) = (at("2026-11-09T00:00:00Z"), at("2026-11-12T00:00:00Z"));
        let mut l = Ledger::default();
        // x until Saturday, then y (Max 5x, a quarter of a Max 20x): their weeks overlap four days.
        week_of(&mut l, x_end, &[(100, &[(&x, 30.0, true)]), (40, &[(&x, 50.0, true)])]);
        week_of(&mut l, y_end, &[(70, &[(&y, 10.0, true), (&x, 50.0, false)]), (2, &[(&y, 30.0, false)])]);
        assert_eq!(weekly(&busiest(&l, Provider::Claude, y_end + hours(1))).0, Some(0.58), "0.5 + 0.3 × 0.25, rounded up");
        // x's next week, its reset an hour's jitter later: right after the first, never both at once.
        let mut l = Ledger::default();
        week_of(&mut l, x_end, &[(100, &[(&x, 60.0, true)])]);
        week_of(&mut l, x_end + days(7) + Duration::minutes(50), &[(100, &[(&x, 50.0, true)])]);
        week_of(&mut l, x_end + days(14) - Duration::minutes(10), &[(100, &[(&x, 20.0, true)])]);
        assert_eq!(l.windows.len(), 3);
        assert_eq!(weekly(&busiest(&l, Provider::Claude, x_end + days(15))).0, Some(0.6));
    }

    #[test]
    fn the_week_in_progress_counts_at_its_pace_after_three_days() {
        let (x, end) = (max20("x"), at("2026-11-30T00:00:00Z"));
        let mut l = Ledger::default();
        // Two days in, 20% used: that use, not its pace.
        let now = end - days(5);
        refresh(&mut l, vec![read(&x, 20.0, end, true, now)], now);
        assert_eq!(weekly(&busiest(&l, Provider::Claude, now)), (Some(0.2), Some(0.2)));
        // Four days in, 40%: on pace for 70%.
        let now = end - days(3);
        refresh(&mut l, vec![read(&x, 40.0, end, true, now)], now);
        assert_eq!(weekly(&busiest(&l, Provider::Claude, now)), (Some(0.7), Some(0.7)));
        // Once it has reset, the week counts at what it reached.
        assert_eq!(weekly(&busiest(&l, Provider::Claude, end + hours(1))), (Some(0.4), None));
    }

    #[test]
    fn pro_200_weeks_begun_before_the_allowance_cut_count_double() {
        let pro = account(Provider::Codex, "c", "pro", None);
        let mut l = Ledger::default();
        // Began 2026-10-26, before the cut (2026-10-30): 30% of the old allowance is 60% of the new.
        week_of(&mut l, at("2026-11-02T00:00:00Z"), &[(50, &[(&pro, 30.0, true)])]);
        assert_eq!(weekly(&busiest(&l, Provider::Codex, at("2026-11-02T01:00:00Z"))).0, Some(0.6));
        let mut l = Ledger::default();
        week_of(&mut l, at("2026-11-09T00:00:00Z"), &[(50, &[(&pro, 30.0, true)])]);
        assert_eq!(weekly(&busiest(&l, Provider::Codex, at("2026-11-09T01:00:00Z"))).0, Some(0.3));
        // Pro 500 counts two and a half plans; a free plan counts nothing and says so.
        let mut l = Ledger::default();
        week_of(&mut l, at("2026-11-09T00:00:00Z"), &[(50, &[(&account(Provider::Codex, "m", "promax", None), 10.0, true), (&account(Provider::Codex, "f", "free", None), 90.0, true)])]);
        let codex = busiest(&l, Provider::Codex, at("2026-11-09T01:00:00Z"));
        assert_eq!((weekly(&codex).0, codex.uncounted), (Some(0.25), true));
    }

    /// A ledger whose provider refreshed on `per_week[i]` days of the week `i + 1` weeks before now's.
    fn refreshed(per_week: &[usize], now: DateTime<Utc>) -> (u8, bool) {
        let this_week = monday(now.date_naive());
        let days: BTreeSet<NaiveDate> = per_week.iter().enumerate()
            .flat_map(|(i, n)| (0..*n as i64).map(move |d| this_week - Duration::weeks(i as i64 + 1) + Duration::days(d))).collect();
        evidence(Some(&days), now)
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
        assert_eq!(evidence(None, now), (0, false));
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

    #[test]
    fn a_load_is_read_leniently_and_never_sinks_its_record() {
        let record = |load: serde_json::Value| serde_json::json!({
            "v": 1, "machine_id": "22222222-2222-4222-8222-222222222222", "machine": "LAPTOP-2",
            "written_at": "2026-11-23T10:00:00Z", "interval_s": 300,
            "seats": [{ "p": "claude", "h": "95ebf9717011b553004c8c1c6a6241f1", "where": ["Windows"] }], "load": load,
        });
        for bad in [serde_json::json!("junk"), serde_json::json!(null), serde_json::json!([1, 2]), serde_json::json!({ "v": 2, "at": "2026-11-23T10:00:00Z", "providers": {} }),
            serde_json::json!({ "v": 1, "providers": {} }), serde_json::json!({ "v": "1", "at": "2026-11-23T10:00:00Z" })] {
            let p: crate::presence::Presence = serde_json::from_value(record(bad.clone())).unwrap();
            assert_eq!((p.seats.len(), p.load.is_none()), (1, true), "{bad}");
        }
        let odd = serde_json::json!({ "v": 1, "at": "2026-11-23T10:00:00Z", "future": true, "providers": {
            "claude": { "busiest": { "weekly": 0.123, "weekly_opus": -3, "weekly_haiku": 0.5, "weekly_sonnet": "x" }, "live": { "weekly": 1000 }, "weeks": 300, "measured": true, "uncounted": "no" },
            "gemini": { "busiest": { "weekly": 0.5 } }, "codex": "junk" } });
        let p: crate::presence::Presence = serde_json::from_value(record(odd)).unwrap();
        let load = p.load.unwrap();
        assert_eq!(load.providers.keys().collect::<Vec<_>>(), ["claude"]);
        let claude = &load.providers["claude"];
        assert_eq!(claude.busiest, BTreeMap::from([("weekly".to_string(), 0.13), ("weekly_opus".to_string(), 0.0)]));
        assert_eq!((claude.live["weekly"], claude.weeks, claude.measured, claude.uncounted), (100.0, 8, true, false));
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
        peers.note(&place(), &[peer("b", "LAPTOP-2", now - hours(1), Some(loaded(&[("weekly", 0.3)], true)), &["x"]), peer("c", "OLD-PC", now, None, &[])], "DESK-1", now);
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
        with_partial.note(&place(), &[peer("b", "LAPTOP-2", now - hours(1), Some(partial), &["x"])], "DESK-1", now);
        let (computers, groups) = shown(&own, &with_partial, &accounts, now);
        assert_eq!((computers.len(), groups.len()), (2, 0));
        // LAPTOP-2 quits: its empty record carries no load, and its last one stands.
        peers.note(&place(), &[peer("b", "LAPTOP-2", now, None, &[])], "DESK-1", now);
        assert_eq!(shown(&own, &peers, &accounts, now).0[1], ("LAPTOP-2".to_string(), Some(30.0), true, false));
        // Until its record is no longer listed: a week after it was written.
        let later = now + Duration::seconds(crate::presence::IGNORE_AFTER + 1);
        peers.note(&place(), &[], "DESK-1", later);
        assert_eq!(shown(&own, &peers, &accounts, later).0.len(), 1);
        // Sharing moved: nothing from the old place.
        let mut peers = Peers::default();
        peers.note(&place(), &[peer("b", "LAPTOP-2", now, Some(loaded(&[("weekly", 0.3)], true)), &[])], "DESK-1", now);
        peers.note(&crate::sharing::Place { folder: Some("D:\\Sync".into()), ..Default::default() }, &[], "DESK-1", now);
        assert_eq!(shown(&own, &peers, &accounts, now).0.len(), 1);
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
    fn names_compare_without_case_or_invisible_characters() {
        let now = at("2026-11-23T10:00:00Z");
        let mut peers = Peers::default();
        let light = Some(loaded(&[("weekly", 0.1)], true));
        peers.note(&place(), &[
            peer("a", "desk-1", now, light.clone(), &[]),
            peer("z", "DESK\u{200B}-1", now, light.clone(), &[]),
            // LAPTOP-2 reinstalled: a new id under the same name, its old record still listed.
            peer("b-old", "LAPTOP-2", now - days(3), Some(loaded(&[("weekly", 0.5)], true)), &[]),
            peer("b-new", "laptop-2\u{FEFF}", now, light.clone(), &[]),
        ], "DESK-1", now);
        let (computers, _) = shown(&loaded(&[("weekly", 0.1)], true), &peers, &[max20("x")], now);
        assert_eq!(computers.iter().map(|c| (c.0.as_str(), c.1)).collect::<Vec<_>>(), [("DESK-1", Some(10.0)), ("laptop-2", Some(10.0))]);
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
        peers.note(&place(), &list, "DESK-1", now);
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

    #[test]
    fn the_ledger_round_trips_and_stays_within_its_size() {
        let dir = TempDir::new();
        let path = dir.0.join(FILE);
        let (x, end) = (max20("x"), at("2026-11-09T00:00:00Z"));
        let mut l = Ledger::default();
        week_of(&mut l, end, &[(100, &[(&x, 12.5, true)])]);
        save_to(&path, &l).unwrap();
        let back = load_from(&path);
        assert_eq!((back.version, back.windows.clone(), back.days.clone()), (VERSION, l.windows.clone(), l.days.clone()));
        std::fs::write(&path, br#"{"version":9,"windows":[],"days":{}}"#).unwrap();
        assert!(load_from(&path).days.is_empty(), "another version starts over");
        std::fs::write(&path, vec![b' '; MAX_FILE as usize + 1]).unwrap();
        assert!(load_from(&path).days.is_empty(), "a file past the cap is not read");
        // The most a ledger holds, every field at its longest, fits the cap as it is.
        let now = at("2026-11-23T12:34:56Z");
        let mut full = Ledger { version: VERSION, ..Default::default() };
        for i in 0..MAX_WINDOWS + 40 {
            full.windows.push(Touched { provider: Provider::Claude, account: account_code(Provider::Claude, &format!("account-{i}")), kind: "weekly_sonnet".into(),
                resets_at: now - Duration::minutes(i as i64 * 271), peak: 33.33, read_at: now - Duration::minutes(i as i64 * 271 + 7), weight: Some(0.25), pro_200: true });
        }
        for p in Provider::ALL { full.days.insert(p.key().into(), (0..KEEP_WEEKS * 7 + 3).map(|d| now.date_naive() - days(d)).collect()); }
        prune(&mut full, now);
        assert!(full.windows.len() <= MAX_WINDOWS && full.days["claude"].len() <= KEEP_WEEKS as usize * 7 + 1);
        let bytes = stored(&full).unwrap();
        assert!(bytes.len() as u64 <= MAX_FILE, "{} bytes", bytes.len());
        assert_eq!(serde_json::from_slice::<Ledger>(&bytes).unwrap().windows.len(), full.windows.len(), "nothing dropped to fit");
        // And should it not fit, the oldest windows go until it does.
        let mut huge = full.clone();
        for _ in 0..3 { huge.windows.extend(full.windows.clone()); }
        let bytes = stored(&huge).unwrap();
        assert!(bytes.len() as u64 <= MAX_FILE && !serde_json::from_slice::<Ledger>(&bytes).unwrap().windows.is_empty());
    }

    #[test]
    fn idle_weeks_and_other_windows_are_not_touched() {
        let x = max20("x");
        let now = at("2026-11-23T10:00:00Z");
        let mut l = Ledger::default();
        let mut acc = read(&x, 0.0, now + days(7), true, now);
        acc.windows.push(UsageWindow { kind: "session".into(), window_seconds: Some(18000), used_percent: 80.0, resets_at: Some(now + hours(3)) });
        acc.windows.push(UsageWindow { kind: "weekly".into(), window_seconds: Some(18000), used_percent: 50.0, resets_at: Some(now + days(3)) });
        refresh(&mut l, vec![acc], now);
        assert!(l.windows.is_empty(), "a week not begun, a session window and one that is not a week");
        // Pinned to a project here, or leased here: in use.
        let mut l = Ledger::default();
        observe(&mut l, &Snapshot { accounts: vec![read(&x, 5.0, now + days(4), false, now)], updated_at: None }, &[(Provider::Claude, "x".into())], &Providers::default(), now);
        assert_eq!(l.windows.len(), 1);
    }
}
