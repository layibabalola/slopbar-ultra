use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::accounts::Provider;

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct UsageWindow {
    /// "session", "weekly", "weekly_opus", "weekly_sonnet" or "window"; translated by the frontend.
    pub kind: String,
    pub window_seconds: Option<i64>,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AccountUsage {
    /// Vault slot id; stable across token refreshes.
    pub id: String,
    pub provider: Provider,
    pub email: Option<String>,
    pub alias: Option<String>,
    pub plan: Option<String>,
    pub org: Option<String>,
    /// Whether this account is the one signed in to the CLI.
    pub active: bool,
    /// Where it is signed in, e.g. "Windows" or "WSL: Ubuntu".
    pub sources: Vec<String>,
    pub windows: Vec<UsageWindow>,
    /// A balance, or "unlimited".
    pub credits: Option<String>,
    /// Limit resets the account holds (granted by Anthropic/OpenAI, used on demand).
    pub resets: Vec<ResetGrant>,
    /// An error code ("token_expired", "token_invalid", "bad_response", "http_<status>")
    /// or a raw network error message.
    pub error: Option<String>,
    /// Independent monitoring can need recovery while live CLI usage is fresh.
    #[serde(default)]
    pub monitor_error: Option<String>,
    /// Last observed numbers; never predict a reset without a successful provider response.
    pub stale: bool,
    pub fetched_at: Option<DateTime<Utc>>,
    pub monitor_connected: bool,
    /// When the monitoring sign-in ends: the date the provider set when it was made (Claude: about
    /// 30 days later). Renewing never moves it; only a new sign-in does. None when the provider
    /// gives none (Codex) or monitoring is not connected.
    #[serde(default)]
    pub monitor_ends_at: Option<DateTime<Utc>>,
    /// Monitoring is connected but needs a new sign-in, and why (see `monitoring::renewal`): it
    /// "ended" (the provider's fixed limit), was "refused" (it can no longer be renewed for another
    /// reason) or is "ending" (within `monitoring::RENEW_DAYS`, at `monitor_ends_at`). "Connect
    /// monitoring for all" and "Sign in all accounts" sign it in again, as they do an unmonitored one.
    #[serde(default)]
    pub monitor_renewal: Option<RenewalKind>,
    /// What the saved login says about the plan's billing (see `Slot::billing`).
    #[serde(default)]
    pub billing: BillingHints,
    /// Other machines that say (through the shared presence folder) this account is signed in
    /// there. `active` and `sources` stay about this machine only.
    #[serde(default)]
    pub remote: Vec<RemoteSeat>,
    /// Every machine the account is in use on right now (this one first, then live other
    /// machines by name), when that is more than one; empty otherwise (see `presence::overlap`).
    /// Two machines on one account drain its limits twice as fast, and the provider may sign
    /// one of them out.
    #[serde(default)]
    pub overlap: Vec<String>,
    /// Another of the user's computers' reading of this account, newer than this computer's own,
    /// while this computer has no fresh reading of its own (see `roster::apply_readings`). Shown
    /// only: switching, alerts, history, forecasts and freshness go by `windows`, `stale` and
    /// `fetched_at`, which stay this computer's own readings.
    #[serde(default)]
    pub shared: Option<SharedReading>,
    /// The other computer whose reading of this account is under `roster::COVERED_HOURS` old (and
    /// seen to move forward that recently here), whether or not this computer reads it too: its
    /// monitoring sign-in covers this one's, so Health calls this one's ending a warning, not a
    /// problem, and the flyout does not count it among the sign-ins to reconnect.
    #[serde(default)]
    pub covered_by: Option<String>,
    /// Usage kept rising while the account was signed in nowhere SlopBar can see: another
    /// machine, the web or the desktop app.
    #[serde(default)]
    pub used_outside: bool,
    /// When the weekly limit runs out at the current rate, when that is before it resets
    /// (`subscriptions::forecast`). Never set from a stale reading.
    #[serde(default)]
    pub runs_out_at: Option<DateTime<Utc>>,
    /// Use seen while none of the user's computers had it signed in (see `unexpected`), when
    /// there was any this week.
    #[serde(default)]
    pub unexpected: Option<UnexpectedUse>,
    /// The provider rejected the CLI's own saved login (its access token got a 401). Cleared as
    /// soon as that login is renewed or works again; the CLI signs in again on its own.
    #[serde(default)]
    pub cli_rejected: bool,
    /// When the account's saved CLI sign-in (its vault slot) ends: the date the provider set when
    /// it was made (Claude: about 30 days later; Claude Code saves it as `refreshTokenExpiresAt`).
    /// Renewing never moves it; only a new sign-in does. None when the saved login does not say
    /// (Codex, or a login saved by an older Claude Code).
    #[serde(default)]
    pub cli_ends_at: Option<DateTime<Utc>>,
    /// The saved CLI sign-in of an account not in use needs a new sign-in (see
    /// `accounts::cli_renewal`): it "ended", so switching the CLI to it would stop the CLI until it
    /// signs in again, or is "ending" (within `monitoring::RENEW_DAYS`, at `cli_ends_at`). "Sign in
    /// all accounts" signs it in again as a new CLI login for the same account. Never set for the
    /// account in use: the CLI manages that sign-in.
    #[serde(default)]
    pub cli_renewal: Option<RenewalKind>,
    /// Claude: pay-as-you-go use beyond the plan's limits this month, when the usage endpoint
    /// reported it (see `providers::claude::extra_usage`).
    #[serde(default)]
    pub extra_usage: Option<ExtraUsage>,
    /// The purpose of the lease reserving this account for a job (see `leases`), set on the
    /// snapshot the app publishes.
    #[serde(default)]
    pub leased: Option<String>,
    /// This machine's desktop app for the provider (Claude Desktop, or the Codex app, which
    /// shares the CLI's sign-in) is signed in to this account (see `desktop_apps`). `active`
    /// stays about the CLI.
    #[serde(default)]
    pub desktop: bool,
    /// Set on the CLI's account and the desktop app's account when they differ on this machine.
    #[serde(default)]
    pub seats: Option<SeatDrift>,
}

/// Why a sign-in needs a new one: a connected account's monitoring sign-in
/// (`AccountUsage::monitor_renewal`), or the saved CLI sign-in of an account not in use
/// (`AccountUsage::cli_renewal`, which is never `Refused`: SlopBar never renews that sign-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RenewalKind {
    /// It ended on the provider's fixed limit: Claude ends each sign-in about 30 days after it is made.
    Ended,
    /// It can no longer be renewed, with no end near.
    Refused,
    /// It ends within `monitoring::RENEW_DAYS`.
    Ending,
}

/// The CLI and the desktop app on this machine are signed in to different accounts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SeatDrift {
    /// "Claude Desktop" or "Codex app".
    pub app: String,
    /// The CLI's account (alias or email).
    pub cli: Option<String>,
    /// The desktop app's account; None when it is one SlopBar does not have.
    pub desktop: Option<String>,
}

/// Claude's extra usage: what the account may spend beyond its plan each month, and what it has.
/// Amounts are in US dollars; the endpoint reports cents (a 40000 limit is $400).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ExtraUsage {
    /// The user turned extra usage on. None when the endpoint did not say.
    pub enabled: Option<bool>,
    pub monthly_limit: Option<f64>,
    pub used: Option<f64>,
    /// Used against the limit, in percent.
    pub utilization: Option<f64>,
}

impl ExtraUsage {
    /// Turned on: it can cost money this month.
    pub fn active(&self) -> bool { self.enabled == Some(true) }
}

/// A monitored account's weekly usage jumped while every computer sharing its status said it
/// was not signed in there: the web or mobile app, or someone else with the login.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct UnexpectedUse {
    /// Separate occasions in the last seven days.
    pub seen_week: u32,
    pub last_seen: DateTime<Utc>,
    /// When the user was last notified about it (at most once every six hours); a new value
    /// means a new notification is due.
    pub alert_at: Option<DateTime<Utc>>,
}

/// An account's usage as another of the user's computers last read it from the provider itself,
/// carried by that computer's encrypted roster (see `roster::Reading`). Advisory: the roster's key
/// is shared by all the user's computers and the name is the one that computer gives itself.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct SharedReading {
    /// That computer's name.
    pub machine: String,
    /// When it read it, by its clock: never later than it wrote its roster, than this computer
    /// first saw it, nor than now.
    pub taken_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
}

/// Where another machine has an account signed in.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemoteSeat {
    /// That machine's name.
    pub machine: String,
    /// Where on it, e.g. "Windows" or "WSL: Ubuntu".
    #[serde(rename = "where")]
    pub where_: Vec<String>,
    /// When it last said so.
    pub seen_at: DateTime<Utc>,
    /// It is still writing; otherwise it went quiet within the last day.
    pub live: bool,
}

/// Plan and billing facts from the saved login and, when SlopBar asked the provider, its own
/// account record. None of them is a promise: token claims are only as current as the token, so
/// the report labels every date with where it came from.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BillingHints {
    /// Claude's rate-limit tier ("default_claude_max_20x"), which tells Max 5x from 20x.
    pub tier: Option<String>,
    /// How the plan is paid ("stripe_subscription"; app stores bill differently).
    pub billing_type: Option<String>,
    /// When the subscription started (Claude) or its current period began (Codex).
    pub started_at: Option<DateTime<Utc>>,
    /// Codex: the end of the paid period, as of `checked_at`.
    pub active_until: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
    /// Codex's account record: the next charge, and the day a cancelled plan ends.
    pub renews_at: Option<DateTime<Utc>>,
    pub cancels_at: Option<DateTime<Utc>>,
    pub will_renew: Option<bool>,
    /// A payment failed and the provider is waiting for it.
    pub delinquent: Option<bool>,
    /// "monthly" or "yearly".
    pub billing_period: Option<String>,
    /// The provider's own name for the plan bought ("chatgptpro"), finer than the plan type.
    pub plan_code: Option<String>,
    /// Codex: the plan type the usage endpoint reports on every poll ("prolite", "pro", "promax"
    /// for Pro 100/200/500). Fresher than the token's claim, which lags a plan change until the
    /// token is reissued.
    pub plan_type: Option<String>,
    /// Claude's organization status ("active").
    pub status: Option<String>,
    /// When SlopBar last read the provider's account record (for the fields above).
    pub fetched_at: Option<DateTime<Utc>>,
}

impl BillingHints {
    /// These hints with every field the provider's own record answered taking precedence.
    pub fn overlay(mut self, provider: &BillingHints) -> BillingHints {
        macro_rules! take { ($($f:ident),*) => { $( if provider.$f.is_some() { self.$f = provider.$f.clone(); } )* } }
        take!(tier, billing_type, started_at, active_until, checked_at, renews_at, cancels_at, will_renew, delinquent, billing_period, plan_code, plan_type, status, fetched_at);
        self
    }
}

impl AccountUsage {
    /// The highest usage across windows: what the balancer compares against its threshold.
    pub fn peak(&self) -> Option<f64> {
        self.windows.iter().map(|w| w.used_percent).reduce(f64::max)
    }
}

/// A usage-limit reset the account can spend to refill its limits.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResetGrant {
    /// "full", "session", "weekly", or "unknown" when the provider scope is unrecognized.
    pub kind: String,
    /// The provider's own title, when it gives one (e.g. "Full reset").
    pub title: Option<String>,
    pub count: u32,
    /// Whether it can be spent right now (usually only once a limit is reached).
    pub usable: bool,
    pub expires_at: Option<DateTime<Utc>>,
    /// For resets that come back periodically: when the next one becomes available.
    pub next_available_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub accounts: Vec<AccountUsage>,
    pub updated_at: Option<DateTime<Utc>>,
}
