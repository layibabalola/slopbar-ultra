//! Another computer's sign-ins, completed in this computer's browser ("Sign in all your computers
//! from here"). The other computer's SlopBar runs the CLI's own sign-in exactly as "Sign in all
//! accounts" runs it there, but with no window and no browser: the CLI's page (the provider's
//! authorization URL) is handed to this computer, which opens an SSH tunnel for the one loopback
//! port that page sends the browser back to, and opens the page here. The provider's answer (the
//! authorization code) goes from this computer's browser through the user's own SSH to the other
//! computer's CLI, which exchanges it itself: the relay never reads, stores or logs the code or a
//! token on either computer, and the login is saved only on the computer whose CLI made it, by that
//! computer's SlopBar as a sign-in at its own screen is.
//!
//! The other computer's half is a [`Slot`]: one relayed sign-in at a time, its page, and how it
//! ended, reached over SSH through `slopbar-mcp --relay` (never an MCP tool, so a page never lands
//! in an agent's result). This computer's half: [`ask`] (what each other computer needs), the
//! checks in [`check_page`], [`Tunnel`] and [`drive`] (one relayed sign-in from start to end).

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::accounts::Provider;
use crate::door::Request;

/// The version of the relay this app speaks, which its readiness and plan say (`relay`): a
/// computer without it cannot have its sign-ins completed elsewhere yet.
pub const VERSION: u32 = 1;

/// The lowest callback port a page may name: never a system port.
const MIN_PORT: u16 = 1024;

// ---------------------------------------------------------------------------------------------
// The page

/// Where a CLI's own authorization page lives and the OAuth client it signs in as. A page that
/// differs is refused (`relay_page_unexpected`) rather than opened: a CLI that changes these fails
/// closed until SlopBar learns the new ones, and a sign-in at the computer's own screen still works.
struct Cli {
    hosts: &'static [&'static str],
    paths: &'static [&'static str],
    client: &'static str,
}

/// Claude Code (2.1.330 sends `claude.com/cai/oauth/authorize`; older ones `claude.ai/oauth/authorize`).
const CLAUDE_CODE: Cli = Cli {
    hosts: &["claude.com", "claude.ai", "platform.claude.com", "console.anthropic.com"],
    paths: &["/cai/oauth/authorize", "/oauth/authorize"],
    client: "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
};

/// Codex's CLI (its app server's `account/login/start`).
const CODEX_CLI: Cli = Cli { hosts: &["auth.openai.com"], paths: &["/oauth/authorize"], client: "app_EMoamEEZ73f0CkXaXp7hrann" };

fn cli(provider: Provider) -> &'static Cli {
    match provider {
        Provider::Claude => &CLAUDE_CODE,
        Provider::Codex => &CODEX_CLI,
    }
}

/// A relayed sign-in's page, checked: the loopback port it sends the browser back to, which is the
/// one port a tunnel for it forwards, and whether it names `localhost` (browsers try that over IPv6
/// first) rather than 127.0.0.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub port: u16,
    pub localhost: bool,
}

/// Checks a page a relayed sign-in for `provider`'s account `email` hands over: the provider's own
/// CLI's authorization page (host, path and OAuth client pinned, see [`Cli`]), an authorization code
/// request with a PKCE S256 challenge and a state, no parameter that would skip or reshape the
/// consent (`prompt`, `request`, `request_uri`, `response_mode`), a login hint that is the step's
/// email (Claude's always names a plain address it was given; none on Codex's), and a `redirect_uri`
/// of `http://localhost:<port>/…` or `http://127.0.0.1:<port>/…` at or above port 1024. Each of
/// those parameters may appear once.
pub fn check_page(url: &str, provider: Provider, email: Option<&str>) -> Result<Page, &'static str> {
    const BAD: &str = "relay_page_unexpected";
    if !crate::signin::allowed_url(url) {
        return Err(BAD);
    }
    let url = reqwest::Url::parse(url).map_err(|_| BAD)?;
    let cli = cli(provider);
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if !cli.hosts.contains(&host.as_str()) || !cli.paths.contains(&url.path()) {
        return Err(BAD);
    }
    let (mut redirect, mut code, mut challenge, mut method, mut state, mut client, mut hint) = (None, None, None, None, None, None, None);
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "prompt" | "request" | "request_uri" | "response_mode" => return Err(BAD),
            "redirect_uri" => &mut redirect,
            "response_type" => &mut code,
            "code_challenge" => &mut challenge,
            "code_challenge_method" => &mut method,
            "state" => &mut state,
            "client_id" => &mut client,
            "login_hint" => &mut hint,
            _ => continue,
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(BAD);
        }
    }
    let filled = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
    if code.as_deref() != Some("code") || method.as_deref() != Some("S256") || !filled(&challenge) || !filled(&state) || client.as_deref() != Some(cli.client) {
        return Err(BAD);
    }
    let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
    if hint.as_deref().is_some_and(|h| !email.is_some_and(|e| same(h, e))) {
        return Err(BAD);
    }
    // Claude Code is given the email to prefill (`--email`) whenever it is a plain address: its
    // page names it then, or it is not that sign-in's page. Codex's pages name none.
    if provider == Provider::Claude && hint.is_none() && crate::signin::prefill_email(email).is_some() {
        return Err(BAD);
    }
    let back = reqwest::Url::parse(&redirect.ok_or(BAD)?).map_err(|_| BAD)?;
    let localhost = matches!(back.host(), Some(url::Host::Domain("localhost")));
    let loopback = localhost || matches!(back.host(), Some(url::Host::Ipv4(std::net::Ipv4Addr::LOCALHOST)));
    if back.scheme() != "http" || !loopback || !back.username().is_empty() || back.password().is_some() || back.query().is_some() || back.fragment().is_some() {
        return Err(BAD);
    }
    match back.port() {
        Some(port) if port >= MIN_PORT => Ok(Page { port, localhost }),
        _ => Err(BAD),
    }
}

// ---------------------------------------------------------------------------------------------
// The other computer's half

/// Where a relayed sign-in is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The CLI is starting; its page is not here yet.
    Starting,
    /// The page waits for the person at the other computer.
    Page,
    Done,
    /// Nothing was saved: no longer needed (switched to, removed, or signed in meanwhile).
    Skipped,
    Failed,
}

impl Phase {
    pub fn ended(self) -> bool {
        matches!(self, Phase::Done | Phase::Skipped | Phase::Failed)
    }
}

/// How long a relayed sign-in goes on without the computer completing it asking after it: that
/// computer asks at least every 65 seconds while it waits and gives up itself after 90 without an
/// answer, so this long without a word means it is gone (quit, lost its network), and the sign-in
/// stops rather than waiting out the CLI's own limit.
pub const UNHEARD_LIMIT: Duration = Duration::from_secs(150);

/// How long the CLI may take to hand over its page (Codex's app server can be a cold start).
pub const PAGE_LIMIT: Duration = Duration::from_secs(60);

/// One relayed sign-in, as `--relay` reports it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// The id the computer completing it gave it, so an answer is never about another one.
    pub step: String,
    pub phase: Phase,
    /// The provider's page, only while it waits (`Phase::Page`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bumped at every change.
    pub revision: u64,
}

struct Open {
    step: String,
    /// The machine id of the computer completing it.
    from: String,
    provider: Provider,
    email: Option<String>,
    /// Ties the page hand-over (`deposit`) to this sign-in. It sits in the CLI's environment, where
    /// any program of the same user can read it, so it tells sign-ins apart rather than guarding.
    nonce: String,
    phase: Phase,
    url: Option<String>,
    /// The revision at which the page came: a caller that has seen it is not sent it again.
    page_revision: u64,
    error: Option<String>,
    started: Instant,
    heard: Instant,
    /// The computer completing it asked for it to stop.
    stop: bool,
}

/// What [`Slot::begin`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begun {
    /// A new sign-in: its nonce.
    New(String),
    /// This step was started already (a start whose answer was lost, asked again): nothing new.
    Again,
}

/// The relayed sign-in on this computer, if any: at most one at a time, and the last one's outcome
/// until the next starts.
#[derive(Default)]
pub struct Slot {
    open: std::sync::Mutex<Option<Open>>,
    revision: std::sync::atomic::AtomicU64,
    changed: tokio::sync::Notify,
}

impl Slot {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Open>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn bump(&self) {
        self.revision.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.changed.notify_waiters();
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Takes the slot for step `step` of computer `from`, for `provider`'s account `email`, unless
    /// a relayed sign-in that has not ended holds it (`signin_busy`). Asked again for the same step
    /// it starts nothing.
    pub fn begin(&self, step: &str, from: &str, provider: Provider, email: Option<&str>) -> Result<Begun, &'static str> {
        let mut open = self.lock();
        match open.as_ref() {
            Some(o) if o.step == step => return Ok(Begun::Again),
            Some(o) if !o.phase.ended() => return Err("signin_busy"),
            _ => {}
        }
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let now = Instant::now();
        *open = Some(Open { step: step.to_owned(), from: from.to_owned(), provider, email: email.map(str::to_owned), nonce: nonce.clone(),
            phase: Phase::Starting, url: None, page_revision: 0, error: None, started: now, heard: now, stop: false });
        drop(open);
        self.bump();
        Ok(Begun::New(nonce))
    }

    /// The step that holds the slot and has not ended, and the computer it is for.
    pub fn holder(&self) -> Option<(String, String)> {
        self.lock().as_ref().filter(|o| !o.phase.ended()).map(|o| (o.step.clone(), o.from.clone()))
    }

    /// The page for the sign-in named by `nonce`, once, when it passes [`check_page`] for its
    /// account. False (and nothing kept) for any other nonce, a second page or a page that fails.
    pub fn deposit(&self, nonce: &str, url: &str) -> bool {
        let mut open = self.lock();
        let Some(o) = open.as_mut().filter(|o| o.nonce == nonce && o.phase == Phase::Starting) else { return false };
        if check_page(url, o.provider, o.email.as_deref()).is_err() {
            return false;
        }
        o.phase = Phase::Page;
        o.url = Some(url.to_owned());
        // Counted under the same lock that sets the page, so any answer either has neither or both:
        // a caller that saw this revision was sent the page with it.
        o.page_revision = self.revision.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        drop(open);
        self.changed.notify_waiters();
        true
    }

    /// Ends step `step` with `phase`; its page is forgotten. Nothing for an ended or another step.
    pub fn finish(&self, step: &str, phase: Phase, error: Option<String>) {
        let mut open = self.lock();
        let Some(o) = open.as_mut().filter(|o| o.step == step && !o.phase.ended()) else { return };
        o.phase = phase;
        o.url = None;
        o.error = error;
        drop(open);
        self.bump();
    }

    /// The state of step `step` (None: another step holds the slot, or none ever did), with its
    /// page; asking is also what keeps it going (see [`UNHEARD_LIMIT`]).
    pub fn state(&self, step: &str) -> Option<State> {
        self.state_since(step, 0)
    }

    /// [`state`](Self::state) for a caller that has seen revision `since`: the page only when it
    /// came after that, so it crosses SSH (and any record kept of a session's output) once.
    fn state_since(&self, step: &str, since: u64) -> Option<State> {
        let mut open = self.lock();
        let o = open.as_mut().filter(|o| o.step == step)?;
        o.heard = Instant::now();
        let url = o.url.clone().filter(|_| o.page_revision > since);
        Some(State { step: o.step.clone(), phase: o.phase, url, error: o.error.clone(), revision: self.revision() })
    }

    /// Waits up to `wait` for step `step` to change after revision `since` (at once when it has, or
    /// has ended), then answers as [`state`](Self::state), with the page only if it came after `since`.
    pub async fn wait_state(&self, step: &str, since: u64, wait: Duration) -> Option<State> {
        let until = tokio::time::Instant::now() + wait;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let ended = self.state_since(step, since).is_none_or(|s| s.phase.ended());
            if self.revision() > since || ended || tokio::time::timeout_at(until, changed).await.is_err() {
                return self.state_since(step, since);
            }
        }
    }

    /// Waits up to `wait` for step `step` to have its page or end, then answers as
    /// [`state`](Self::state): a start answers with the page when it comes soon.
    pub async fn wait_page(&self, step: &str, wait: Duration) -> Option<State> {
        let until = tokio::time::Instant::now() + wait;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let waiting = self.state(step).is_some_and(|s| s.phase == Phase::Starting);
            if !waiting || tokio::time::timeout_at(until, changed).await.is_err() {
                return self.state(step);
            }
        }
    }

    /// Asks step `step` to stop; false when it is not open.
    pub fn stop(&self, step: &str) -> bool {
        let mut open = self.lock();
        let Some(o) = open.as_mut().filter(|o| o.step == step && !o.phase.ended()) else { return false };
        o.stop = true;
        drop(open);
        self.bump();
        true
    }

    /// Asks whatever relayed sign-in is open to stop (this computer's own Stop); whether one was.
    pub fn stop_any(&self) -> bool {
        let step = self.holder().map(|(step, _)| step);
        step.is_some_and(|step| self.stop(&step))
    }

    /// Why step `step` should stop, if it should: the computer completing it asked
    /// (`signin_stopped`), stopped asking after it (`relay_unheard`, see [`UNHEARD_LIMIT`]), or its
    /// page never came (`relay_no_page`, see [`PAGE_LIMIT`]).
    pub fn should_stop(&self, step: &str) -> Option<&'static str> {
        let open = self.lock();
        let o = open.as_ref().filter(|o| o.step == step && !o.phase.ended())?;
        if o.stop {
            Some("signin_stopped")
        } else if o.heard.elapsed() >= UNHEARD_LIMIT {
            Some("relay_unheard")
        } else if o.phase == Phase::Starting && o.started.elapsed() >= PAGE_LIMIT {
            Some("relay_no_page")
        } else {
            None
        }
    }

    /// Whether a relayed sign-in that has not ended holds the slot.
    pub fn busy(&self) -> bool {
        self.lock().as_ref().is_some_and(|o| !o.phase.ended())
    }
}

/// The app's slot: the page hand-over reaches it from the CLI's `BROWSER` start through the door
/// and from Codex's app server in the app itself.
pub static SLOT: std::sync::LazyLock<Slot> = std::sync::LazyLock::new(Slot::default);

/// Ends a relayed sign-in's step as failed (`relay_failed`) if its task ends without saying how
/// (a panic, or the task dropped): the slot is never left held.
pub struct EndGuard<'a> {
    pub slot: &'a Slot,
    pub step: String,
}

impl Drop for EndGuard<'_> {
    fn drop(&mut self) {
        self.slot.finish(&self.step, Phase::Failed, Some("relay_failed".into()));
    }
}

// ---------------------------------------------------------------------------------------------
// This computer's half: which of the user's other computers take part, and what each needs

/// A computer (or one of its sign-ins) left out of a sitting, and why: `reason` is the error code,
/// `why` the words the windows show for it (see [`why`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub machine: String,
    pub reason: String,
    pub why: &'static str,
    /// One sign-in left out, not the whole computer: the account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

impl Skipped {
    pub fn computer(machine: &str, reason: &str) -> Self {
        let reason = known(reason);
        Skipped { machine: machine.to_owned(), why: why(&reason), reason, account: None }
    }
}

/// Which accounts another computer reads usage for itself, rather than only showing what the
/// user's other computers read: those its automatic switching (`auto` per provider) or a rotation
/// threshold (`rotate`, `provider:id`) acts on. Its monitoring sign-ins for the rest are left out
/// of a sitting when this computer reads those accounts itself (see `share_readings`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnReadings {
    pub claude: bool,
    pub codex: bool,
    pub rotate: Vec<String>,
}

impl OwnReadings {
    pub fn uses(&self, provider: Provider, id: &str) -> bool {
        let auto = match provider { Provider::Claude => self.claude, Provider::Codex => self.codex };
        auto || self.rotate.iter().any(|k| *k == format!("{}:{id}", provider.key()))
    }
}

/// An account in use on a computer whose CLI sign-in ended or ends within the renewal horizon: the
/// CLI manages that sign-in, so no sitting renews it, and the user is told (`claude /login` there).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InUse {
    #[serde(default)]
    pub machine: String,
    pub provider: Provider,
    pub id: String,
    pub email: Option<String>,
    pub ends_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Another computer's accounts in use whose sign-ins end, as the person is shown them: only those
/// this computer knows (`known`: provider, id, email, its own or the rosters'), with this
/// computer's id and email for them, so nothing that computer wrote is shown as it wrote it.
pub fn known_in_use(in_use: &[InUse], known: &[(Provider, String, Option<String>)]) -> Vec<InUse> {
    let same = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
        (None, None) => true,
        _ => false,
    };
    in_use.iter().filter_map(|u| {
        let (_, id, email) = known.iter().find(|(p, i, e)| *p == u.provider && *i == u.id && same(e, &u.email))?;
        Some(InUse { id: id.clone(), email: email.clone(), ..u.clone() })
    }).collect()
}

/// What becomes of one of another computer's sign-ins in a sitting (see [`keep`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Yes,
    /// An account this computer does not know (its own, or a roster's, with the same email): its
    /// page is never opened here, whatever that computer says.
    Unknown,
    /// A monitoring sign-in that computer need not make: this computer's shared readings cover it.
    Covered,
}

/// Whether `step`, one of computer `offer`'s sign-ins, is part of a sitting here. `known` are the
/// accounts this computer knows (provider, id, email); `reads_here` whether this computer reads an
/// account's usage itself; `shares_with_it` whether this computer's readings reach that computer
/// (readings shared here, and that computer reads this one's roster).
pub fn keep(step: &crate::bulk::Step, offer: &Offer, known: &[(Provider, String, Option<String>)], reads_here: impl Fn(Provider, &str) -> bool, shares_with_it: bool) -> Keep {
    let (provider, id) = (step.target.provider, step.target.id.as_str());
    let same = |a: &Option<String>| match (a, &step.email) {
        (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
        (None, None) => true,
        _ => false,
    };
    if !known.iter().any(|(p, i, e)| *p == provider && i == id && same(e)) {
        return Keep::Unknown;
    }
    if step.kind == crate::bulk::Kind::Connect && shares_with_it && reads_here(provider, id) && !offer.own.uses(provider, id) {
        return Keep::Covered;
    }
    Keep::Yes
}

/// What another computer's `RelayPlan` answers (see `app::relay_plan`).
#[derive(Debug, Clone, Deserialize)]
struct PlanAnswer {
    machine: Option<MachineAnswer>,
    relay: Option<u32>,
    #[serde(default)]
    plan: Vec<crate::bulk::Group>,
    #[serde(default)]
    own_readings: OwnReadings,
    #[serde(default)]
    in_use: Vec<InUse>,
    #[serde(default)]
    busy: bool,
    #[serde(default)]
    ssh_shell: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MachineAnswer {
    id: String,
    name: String,
}

/// One computer's part in a sitting, as the person confirms it: keyed by machine id, so two
/// computers that give the same name never merge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Computer {
    pub id: String,
    pub name: String,
    /// The SSH host that reaches it; None for this computer.
    pub host: Option<String>,
    /// Its sign-ins in the sitting.
    pub steps: usize,
    /// The emails they sign in as, each once.
    pub emails: Vec<String>,
    /// Sign-ins for accounts without an email.
    pub unnamed: usize,
}

/// The computers of a sitting's plan, in the order their first sign-in runs: `here` (this
/// computer's id and name) for steps that name no computer.
pub fn computers(groups: &[crate::bulk::Group], here: (&str, &str)) -> Vec<Computer> {
    let mut list: Vec<Computer> = Vec::new();
    for step in groups.iter().flat_map(|g| g.steps.iter()) {
        let (id, name, host) = match &step.machine {
            Some(m) => (m.id.as_str(), m.name.as_str(), Some(m.host.clone())),
            None => (here.0, here.1, None),
        };
        let at = match list.iter().position(|c| c.id == id) {
            Some(at) => at,
            None => {
                list.push(Computer { id: id.to_owned(), name: name.to_owned(), host, steps: 0, emails: vec![], unnamed: 0 });
                list.len() - 1
            }
        };
        let c = &mut list[at];
        c.steps += 1;
        match step.email.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            Some(email) if !c.emails.iter().any(|e| e.eq_ignore_ascii_case(email)) => c.emails.push(email.to_owned()),
            Some(_) => {}
            None => c.unnamed += 1,
        }
    }
    list
}

/// One computer's part in a sitting, as it answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Offer {
    pub machine: crate::bulk::Machine,
    pub plan: Vec<crate::bulk::Group>,
    pub own: OwnReadings,
    pub in_use: Vec<InUse>,
}

/// What asking one computer found.
#[derive(Debug, Clone, PartialEq)]
pub enum Asked {
    /// The host leads back to this computer.
    Here,
    Offer(Offer),
    Skipped(Skipped),
}

/// A name another computer gives itself, as it is shown here: without the characters that hide
/// (see `roster::invisible`) or control anything, trimmed, at most 64 characters; `host` when that
/// leaves nothing.
pub fn shown_name(name: &str, host: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control() && !crate::roster::invisible(*c)).take(64).collect();
    let clean = clean.trim();
    if clean.is_empty() { host.to_owned() } else { clean.to_owned() }
}

/// How long another computer may take to make its plan (beyond SSH's own time, see
/// `pairing::relay_call`).
const PLAN_WAIT: Duration = Duration::from_secs(10);

/// Asks the computer at `host` whether its sign-ins can be completed here and which: one
/// `RelayPlan` call, which answers who it is, the plan its own "Sign in all accounts" would run now
/// (`monitoring` as the switch here says), what it reads usage for itself, its accounts in use whose
/// sign-ins end, and whether it is busy. When that call fails, its readiness says why. `expected` is
/// the machine id `host` was remembered for (another answering is `wrong_machine`); `here` is this
/// computer's. Its screen does not matter (locked, or nobody at it): nothing opens there.
pub fn ask(remote: &dyn crate::pairing::Remote, host: &str, expected: Option<&str>, here: &str, monitoring: bool) -> Asked {
    let skip = |machine: &str, reason: &str| Asked::Skipped(Skipped::computer(machine, reason));
    let answer = crate::pairing::relay_call(remote, host, &Request::RelayPlan { monitoring, from: here.to_owned() }, PLAN_WAIT);
    let answer = match answer {
        Ok(answer) => answer,
        Err(e) if e.starts_with("ssh_") || e == "bad_ssh_host" || e == "no_pwsh_over_ssh" || e == "slopbar_not_installed" => return skip(host, &e),
        // No relay there (an older SlopBar prints nothing), or no app: its readiness says which.
        Err(e) => return diagnose(remote, host, &e),
    };
    let Ok(answer) = serde_json::from_value::<PlanAnswer>(answer) else { return skip(host, "remote_no_answer") };
    let Some(machine) = answer.machine else { return skip(host, "app_not_running") };
    if machine.id == here {
        return Asked::Here;
    }
    let name = shown_name(&machine.name, host);
    if expected.is_some_and(|id| id != machine.id) {
        return skip(&name, "wrong_machine");
    }
    if answer.relay.is_none_or(|v| v < VERSION) {
        return skip(&name, "relay_outdated");
    }
    // Every relay call there would start Windows PowerShell around it (see `pairing::remote_command`).
    if answer.ssh_shell.as_deref() == Some("windows_powershell") {
        return skip(&name, "ssh_shell_windows_powershell");
    }
    if answer.busy {
        return skip(&name, "peer_busy");
    }
    // As that computer gave them: only those this computer knows are shown, as it knows them
    // (see `known_in_use`).
    let in_use = answer.in_use.into_iter().map(|u| InUse { machine: name.clone(), ..u }).collect();
    Asked::Offer(Offer { machine: crate::bulk::Machine { id: machine.id, name, host: host.to_owned() }, plan: answer.plan, own: answer.own_readings, in_use })
}

/// Why a computer did not answer a `RelayPlan`, from its readiness (which every SlopBar answers).
fn diagnose(remote: &dyn crate::pairing::Remote, host: &str, error: &str) -> Asked {
    let skip = |machine: &str, reason: &str| Asked::Skipped(Skipped::computer(machine, reason));
    let Ok(ready) = crate::pairing::remote_readiness(remote, host, &serde_json::json!({})) else { return skip(host, error) };
    let name = ready.pointer("/machine/name").and_then(Value::as_str).map(|n| shown_name(n, host)).unwrap_or_else(|| host.to_owned());
    if ready.pointer("/app/running") == Some(&Value::Bool(true)) {
        // Running, but it did not answer the relay: too old to speak it.
        return skip(&name, if ready["relay"].as_u64().is_some_and(|v| v >= u64::from(VERSION)) { error } else { "relay_outdated" });
    }
    let problems = ready["problems"].as_array().cloned().unwrap_or_default();
    let reason = if ready.pointer("/app/installed") == Some(&Value::Bool(false)) {
        "slopbar_not_installed"
    } else if problems.iter().any(|p| p == "no_desktop_session") {
        "no_desktop_session"
    } else {
        "app_not_running"
    };
    skip(&name, reason)
}

/// How long asking every computer may take: one that has not answered by then is left out. Under
/// the door's 55 seconds, so an agent's preview always gets its answer.
pub const ASK_LIMIT: Duration = Duration::from_secs(45);

/// Asks every host at once (see [`ask`]): `(host, expected machine id)`. One that has not answered
/// within `limit` is left out as `ssh_timeout` (its SSH call ends on its own bound). Answers in
/// `hosts`' order.
pub fn ask_all<R: crate::pairing::Remote + Send + Sync + 'static>(remote: std::sync::Arc<R>, hosts: &[(String, Option<String>)], here: &str, monitoring: bool, limit: Duration) -> Vec<Asked> {
    let (send, receive) = std::sync::mpsc::channel();
    for (at, (host, expected)) in hosts.iter().enumerate() {
        let (remote, host, expected, here, send) = (remote.clone(), host.clone(), expected.clone(), here.to_owned(), send.clone());
        std::thread::spawn(move || {
            let _ = send.send((at, ask(remote.as_ref(), &host, expected.as_deref(), &here, monitoring)));
        });
    }
    drop(send);
    let mut answers: Vec<Option<Asked>> = vec![None; hosts.len()];
    let until = Instant::now() + limit;
    while answers.iter().any(Option::is_none) {
        match receive.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok((at, asked)) => answers[at] = Some(asked),
            Err(_) => break,
        }
    }
    answers.into_iter().zip(hosts).map(|(asked, (host, _))| asked.unwrap_or_else(|| Asked::Skipped(Skipped::computer(host, "ssh_timeout")))).collect()
}

// ---------------------------------------------------------------------------------------------
// What a relayed sign-in's failure means for the sitting

/// What a relayed sign-in's error does to the sitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The person stopped it (Stop, closing the window, another account), or this computer cannot
    /// go on (its screen locked, no browser): the run stops, as a sign-in at its own screen does.
    StopRun,
    /// That computer cannot go on: its remaining sign-ins are left out, the others go on.
    DropComputer,
    /// Only this sign-in is left out.
    SkipStep,
}

/// The codes a relayed sign-in's error may carry here; anything else from another computer reads
/// `relay_failed`, so no free text from it reaches a result, a notification or the roster.
const KNOWN: &[&str] = &[
    "signin_stopped", "signin_window_closed", "monitor_wrong_account", "monitor_wrong_workspace", "desktop_locked", "desktop_not_shown",
    "no_desktop_session", "signin_browser_missing", "signin_profile_storage", "signin_url_rejected", "relay_port_unusable", "monitor_login_failed",
    "monitor_storage", "relay_unknown_account", "relay_lost", "relay_unheard", "relay_no_page", "relay_unsupported", "relay_page_unexpected",
    "relay_outdated", "relay_step_unknown", "relay_stopped_there", "wrong_machine", "peer_busy", "signin_busy", "monitor_busy", "CLI not found", "tunnel_failed",
    "app_not_running", "slopbar_not_installed", "not_running", "door_unavailable", "timeout", "ssh_timeout", "ssh_missing", "ssh_failed",
    "bad_ssh_host", "remote_no_answer", "no_pwsh_over_ssh", "ssh_shell_windows_powershell", "no_ssh_host", "relay_failed",
];

/// `code` as it may be kept and shown here: one of [`KNOWN`], else `relay_failed`. ssh's own
/// failure loses its line: an exit of 255 with text on standard error may come from the command
/// the other computer ran, not from ssh here.
pub fn known(code: &str) -> String {
    if code.starts_with("ssh_failed") {
        "ssh_failed".into()
    } else if KNOWN.contains(&code) {
        code.into()
    } else {
        "relay_failed".into()
    }
}

/// What `code` (see [`known`]) does to the sitting.
pub fn fault(code: &str) -> Fault {
    match code {
        "signin_stopped" | "signin_window_closed" | "monitor_wrong_account" | "monitor_wrong_workspace" | "desktop_locked" | "desktop_not_shown"
        | "signin_browser_missing" | "signin_profile_storage" => Fault::StopRun,
        "relay_port_unusable" | "monitor_login_failed" | "monitor_storage" | "relay_unknown_account" => Fault::SkipStep,
        _ => Fault::DropComputer,
    }
}

/// The words the windows show for a computer or sign-in left out with `code`: an i18n key.
pub fn why(code: &str) -> &'static str {
    match code {
        c if c.starts_with("ssh_") || matches!(c, "bad_ssh_host" | "relay_lost" | "remote_no_answer" | "tunnel_failed") => "leftUnreachable",
        "app_not_running" | "no_desktop_session" | "not_running" | "door_unavailable" | "timeout" => "leftNotRunning",
        "slopbar_not_installed" => "leftNotInstalled",
        "no_ssh_host" => "leftNoHost",
        "relay_outdated" | "relay_unsupported" => "leftOutdated",
        "peer_busy" | "signin_busy" | "monitor_busy" => "leftBusy",
        "no_pwsh_over_ssh" => "leftNoPwsh",
        "ssh_shell_windows_powershell" => "leftShell",
        "wrong_machine" => "leftWrongMachine",
        "CLI not found" => "leftNoCli",
        "relay_port_unusable" => "leftPort",
        "relay_stopped_there" => "leftStoppedThere",
        "relay_unknown_account" => "leftUnknownAccount",
        _ => "leftFailed",
    }
}

// ---------------------------------------------------------------------------------------------
// This computer's half: the tunnel

/// An SSH tunnel for one relayed sign-in's callback port: the loopback address(es) here forward to
/// the other computer's 127.0.0.1 on the same port, where its CLI listens; never any other address
/// or port. Ends when dropped (and with this app: it is in the app's kill-on-close job).
pub struct Tunnel {
    child: std::process::Child,
    #[cfg(windows)]
    _tree: crate::login::ProcessTree,
}

/// How long the tunnel may take to connect and carry a request through.
const TUNNEL_READY: Duration = Duration::from_secs(25);

/// What could not be held: a port this computer cannot use (`relay_port_unusable`), or the tunnel
/// (`tunnel_failed`).
pub type Unheld = &'static str;

impl Tunnel {
    /// Opens it for `page` once nothing here listens on its port on any address (or Windows reserves
    /// it), and returns once a request through it reaches the CLI there (any HTTP answer to a path
    /// the CLI does not serve). Both loopbacks for a `localhost` page where this computer has IPv6,
    /// else IPv4 alone. Fails with `relay_port_unusable` or `tunnel_failed`.
    pub fn open(ssh: &std::path::Path, host: &str, page: &Page) -> Result<Tunnel, Unheld> {
        if !crate::pairing::valid_host(host) || page.port < MIN_PORT {
            return Err("tunnel_failed");
        }
        let port = page.port;
        if !port_free(port) {
            return Err("relay_port_unusable");
        }
        let ipv6 = page.localhost && ipv6_loopback();
        let mut command = std::process::Command::new(ssh);
        command.args(["-N", "-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=15",
            "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=5", "-o", "ServerAliveCountMax=3"]);
        for arg in forwards(port, ipv6) {
            command.arg("-L").arg(arg);
        }
        command.arg(host).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        // Started suspended and put in the kill-on-close job before it runs (nothing it starts,
        // such as a ProxyCommand, can begin outside it), or not at all: a tunnel never outlives
        // this app.
        #[cfg(windows)]
        let (child, tree) = crate::login::ProcessTree::spawn(&mut command, false).map_err(|_| "tunnel_failed")?;
        #[cfg(not(windows))]
        let child = command.spawn().map_err(|_| "tunnel_failed")?;
        let mut tunnel = Tunnel { child, #[cfg(windows)] _tree: tree };
        let started = Instant::now();
        loop {
            if !tunnel.alive() {
                // A reserved port, or one taken meanwhile, ends ssh at its forward; so does SSH.
                return Err(if port_free(port) { "tunnel_failed" } else { "relay_port_unusable" });
            }
            let through = reaches(std::net::Ipv4Addr::LOCALHOST.into(), port) && (!ipv6 || reaches(std::net::Ipv6Addr::LOCALHOST.into(), port));
            // Only ssh listens there (a table that could not be read proves nothing).
            if through && listeners(port).is_some_and(|pids| pids.iter().all(|pid| *pid == tunnel.child.id())) {
                return Ok(tunnel);
            }
            if started.elapsed() > TUNNEL_READY {
                return Err("tunnel_failed");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// Whether ssh still runs.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// ssh's `-L` arguments for `port`: loopback here to the other computer's IPv4 loopback.
fn forwards(port: u16, ipv6: bool) -> Vec<String> {
    let mut list = vec![format!("127.0.0.1:{port}:127.0.0.1:{port}")];
    if ipv6 {
        list.push(format!("[::1]:{port}:127.0.0.1:{port}"));
    }
    list
}

/// Whether this computer has an IPv6 loopback to listen on.
fn ipv6_loopback() -> bool {
    std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// Whether nothing here listens on `port`, on any address of either family (as far as Windows'
/// listener table says: one that could not be read counts as taken), and Windows lets a program
/// listen on its loopback there (a port in an excluded range is refused).
fn port_free(port: u16) -> bool {
    listeners(port).is_some_and(|pids| pids.is_empty()) && std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

/// Whether an HTTP request through `port` on `addr` gets an HTTP answer: the tunnel is up and
/// reaches a listener there. The path is one no CLI serves (Claude Code and Codex answer 404).
fn reaches(addr: std::net::IpAddr, port: u16) -> bool {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&(addr, port).into(), Duration::from_secs(2)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(4)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    if stream.write_all(b"GET /slopbar-relay-check HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
        return false;
    }
    let mut head = [0u8; 9];
    stream.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.")
}

/// The processes listening on `port` here, on any address of either family; None when Windows'
/// table of them could not be read.
#[cfg(windows)]
fn listeners(port: u16) -> Option<Vec<u32>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, TCP_TABLE_OWNER_PID_LISTENER};
    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    const NO_ROOM: u32 = 122; // ERROR_INSUFFICIENT_BUFFER: the table grew between the two calls.
    let mut found = Vec::new();
    // Each row's local port is in network order in the low 16 bits of a u32; rows are u32s after
    // the count: IPv4 (state, address, port, remote address, remote port, pid), IPv6 (address 16
    // bytes, scope, port, remote address 16 bytes, remote scope, remote port, state, pid).
    for (family, row, at_port, at_pid) in [(AF_INET, 24usize, 8usize, 20usize), (AF_INET6, 56, 20, 52)] {
        let mut size = 0u32;
        unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0) };
        let mut buf;
        let mut tries = 0;
        loop {
            buf = vec![0u32; (size as usize).div_ceil(4) + 64];
            let mut room = (buf.len() * 4) as u32;
            match unsafe { GetExtendedTcpTable(buf.as_mut_ptr().cast(), &mut room, 0, family, TCP_TABLE_OWNER_PID_LISTENER, 0) } {
                0 => break,
                NO_ROOM if tries < 3 => {
                    tries += 1;
                    size = room;
                }
                _ => return None,
            }
        }
        let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 4) };
        let word = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]));
        let count = word(0).unwrap_or(0) as usize;
        for i in 0..count {
            let base = 4 + i * row;
            let (Some(local), Some(pid)) = (word(base + at_port), word(base + at_pid)) else { break };
            if u16::from_be(local as u16) == port {
                found.push(pid);
            }
        }
    }
    Some(found)
}

#[cfg(not(windows))]
fn listeners(_: u16) -> Option<Vec<u32>> {
    Some(Vec::new())
}

// ---------------------------------------------------------------------------------------------
// This computer's half: one relayed sign-in from start to end

/// What a relayed sign-in needs from this computer, so tests can stand in for SSH, the tunnel and
/// the browser.
pub trait Desk {
    /// Sends `request` to `host`'s `slopbar-mcp --relay`, allowing `wait` for the waiting it asks
    /// for (see `pairing::relay_call`).
    fn call(&self, host: &str, request: &Request, wait: Duration) -> Result<Value, String>;
    /// Opens a tunnel for `page` to `host`.
    fn tunnel(&self, host: &str, page: &Page) -> Result<Box<dyn Hold>, Unheld>;
    /// Opens the checked page in this computer's browser, in the email's window.
    fn open(&self, url: &str) -> Result<(), String>;
}

/// A tunnel held open.
pub trait Hold: Send {
    fn alive(&mut self) -> bool;
}

impl Hold for Tunnel {
    fn alive(&mut self) -> bool {
        Tunnel::alive(self)
    }
}

/// Who asks whom for a relayed sign-in.
#[derive(Debug, Clone)]
pub struct Ids {
    /// This computer's machine id.
    pub from: String,
    /// The other computer's, as it answered when asked for its plan.
    pub machine: String,
}

/// How long a relayed start waits there for the page before answering.
const START_WAIT: Duration = Duration::from_secs(40);
/// How long a state call waits there for a change.
const STATE_WAIT: u32 = 40;
/// How long the other computer may go unanswered before it counts as lost: inside its own
/// [`UNHEARD_LIMIT`], so it stops the sign-in after this computer has given up, never before.
const LOST_AFTER: Duration = Duration::from_secs(90);
/// How long a start keeps trying a computer that does not answer (restarting into an update).
const START_RETRY: Duration = Duration::from_secs(90);
/// How many times a sign-in is started again for a new port when this computer cannot use the one
/// its CLI picked (Windows reserves some): Claude Code picks a new one each time; Codex's is fixed.
const PORT_TRIES: u32 = 3;

/// Everything one relayed sign-in holds here: its tunnel, whether it was asked to stop, and the
/// step it is on there. Dropping the [`Relayed`] that owns it closes the tunnel at once and asks
/// the other computer to stop that step.
#[derive(Default)]
pub struct Shared {
    tunnel: std::sync::Mutex<Option<Box<dyn Hold>>>,
    cancel: std::sync::atomic::AtomicBool,
    step: std::sync::Mutex<Option<String>>,
    /// The other computer said the step ended (done, not needed or failed there): nothing to stop.
    ended_there: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Runs one relayed sign-in (`step`, for the computer at `host`) from its start to its end: starts
/// it there, opens a tunnel for the page's port when it comes and the page here (`opened` then),
/// keeps the tunnel up, and follows it until it is done (true), not needed (false) or failed (an
/// error code from [`known`]). Blocks; `shared` is how another thread stops it.
pub fn drive(desk: &dyn Desk, host: &str, ids: &Ids, step: &crate::bulk::Step, shared: &Shared, opened: &dyn Fn()) -> Result<bool, String> {
    drive_timed(desk, host, ids, step, shared, opened, &Timing::default())
}

/// [`drive`]'s time limits, which tests shorten.
pub struct Timing {
    pub lost_after: Duration,
    pub start_retry: Duration,
    pub pause: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing { lost_after: LOST_AFTER, start_retry: START_RETRY, pause: Duration::from_secs(3) }
    }
}

fn drive_timed(desk: &dyn Desk, host: &str, ids: &Ids, step: &crate::bulk::Step, shared: &Shared, opened: &dyn Fn(), timing: &Timing) -> Result<bool, String> {
    let provider = step.target.provider;
    let email = step.email.as_deref();
    let mut tries = 0;
    loop {
        // Never a new sign-in there once this one was stopped here.
        if shared.cancelled() {
            return Err("signin_stopped".into());
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        *shared.step.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
        let start = Request::RelayStart {
            step: id.clone(), from: ids.from.clone(), machine: ids.machine.clone(), kind: step.kind,
            provider, id: step.target.id.clone(), email: step.email.clone(),
        };
        let mut state = start_there(desk, host, &start, shared, timing)?;
        let mut last_heard = Instant::now();
        let mut reopened = false;
        // The page, as it came (it is sent once).
        let mut page_url: Option<String> = None;
        loop {
            if state.phase.ended() {
                shared.ended_there.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            // Stopped here while it runs there: stopped there in order, after its start.
            if shared.cancelled() {
                if !state.phase.ended() {
                    let _ = desk.call(host, &Request::RelayStop { step: id.clone() }, Duration::ZERO);
                }
                return Err("signin_stopped".into());
            }
            if state.url.is_some() {
                page_url = state.url.clone();
            }
            match state.phase {
                Phase::Done => return Ok(true),
                Phase::Skipped => return Ok(false),
                // Stopped there, not here (its own Stop, or another sitting's start): that computer
                // is left out, the run goes on.
                Phase::Failed if state.error.as_deref() == Some("signin_stopped") => return Err("relay_stopped_there".into()),
                Phase::Failed => return Err(known(state.error.as_deref().unwrap_or("relay_failed"))),
                Phase::Starting => {}
                Phase::Page => {
                    let held = shared.tunnel.lock().unwrap_or_else(|e| e.into_inner()).is_some();
                    if !held {
                        let Some(url) = page_url.clone() else { return Err("relay_failed".into()) };
                        let page = check_page(&url, provider, email).map_err(String::from)?;
                        match desk.tunnel(host, &page) {
                            Ok(tunnel) => *shared.tunnel.lock().unwrap_or_else(|e| e.into_inner()) = Some(tunnel),
                            // This computer cannot use the port the CLI there picked: a new sign-in
                            // there picks another (Claude Code), else only this sign-in is left out.
                            Err("relay_port_unusable") if provider == Provider::Claude && tries + 1 < PORT_TRIES => {
                                tries += 1;
                                let _ = desk.call(host, &Request::RelayStop { step: id.clone() }, Duration::ZERO);
                                break;
                            }
                            Err(e) => return Err(e.into()),
                        }
                        if shared.cancelled() {
                            return Err("signin_stopped".into());
                        }
                        desk.open(&url)?;
                        opened();
                    }
                }
            }
            // A tunnel that died is opened again once; twice is the computer's fault.
            let dead = shared.tunnel.lock().unwrap_or_else(|e| e.into_inner()).as_mut().is_some_and(|t| !t.alive());
            if dead {
                if reopened {
                    return Err("tunnel_failed".into());
                }
                reopened = true;
                *shared.tunnel.lock().unwrap_or_else(|e| e.into_inner()) = None;
                let url = page_url.clone().ok_or("tunnel_failed")?;
                let page = check_page(&url, provider, email).map_err(String::from)?;
                let tunnel = desk.tunnel(host, &page).map_err(String::from)?;
                // Kept first, then the stop looked at: a Stop that comes between them (which sets
                // the stop before it takes the tunnel) still closes this one.
                *shared.tunnel.lock().unwrap_or_else(|e| e.into_inner()) = Some(tunnel);
                if shared.cancelled() {
                    shared.tunnel.lock().unwrap_or_else(|e| e.into_inner()).take();
                    let _ = desk.call(host, &Request::RelayStop { step: id.clone() }, Duration::ZERO);
                    return Err("signin_stopped".into());
                }
            }
            let ask = Request::RelayState { step: id.clone(), since: state.revision, wait_seconds: STATE_WAIT };
            match desk.call(host, &ask, Duration::from_secs(u64::from(STATE_WAIT))) {
                Ok(answer) => match serde_json::from_value::<State>(answer) {
                    Ok(next) if next.step == id => {
                        state = next;
                        last_heard = Instant::now();
                    }
                    _ => return Err("relay_failed".into()),
                },
                Err(e) if e == "relay_step_unknown" => return Err(e),
                Err(_) if last_heard.elapsed() > timing.lost_after => return Err("relay_lost".into()),
                Err(_) => std::thread::sleep(timing.pause),
            }
        }
    }
}

/// Starts `start` there: its first state, with the page when it came within [`START_WAIT`]. A
/// computer that does not answer (restarting into an update) is asked again for [`START_RETRY`]; a
/// sign-in no longer needed there answers `Skipped`.
fn start_there(desk: &dyn Desk, host: &str, start: &Request, shared: &Shared, timing: &Timing) -> Result<State, String> {
    let until = Instant::now() + timing.start_retry;
    loop {
        if shared.cancelled() {
            return Err("signin_stopped".into());
        }
        match desk.call(host, start, START_WAIT) {
            Ok(answer) => return serde_json::from_value::<State>(answer).map_err(|_| "relay_failed".to_string()),
            Err(e) => {
                let e = known(&e);
                let passing = e.starts_with("ssh_") || matches!(e.as_str(), "not_running" | "door_unavailable" | "timeout" | "remote_no_answer");
                if !passing || Instant::now() > until || shared.cancelled() {
                    return Err(e);
                }
                std::thread::sleep(timing.pause);
            }
        }
    }
}

/// A relayed sign-in started with [`drive`] on a thread of its own. Dropping it (when it ended,
/// or on Stop, the browser closed, this app quitting) closes its tunnel at once and, unless the
/// other computer said the step ended there, asks it to stop: a step left open there would keep
/// that computer's one sign-in for minutes (see [`UNHEARD_LIMIT`]).
pub struct Relayed {
    pub shared: std::sync::Arc<Shared>,
    host: String,
    stop: std::sync::Arc<dyn Fn(&str, &str) + Send + Sync>,
}

impl Relayed {
    /// `stop(host, step)` asks the other computer to stop step `step` (on a thread of its own).
    pub fn new(host: &str, stop: std::sync::Arc<dyn Fn(&str, &str) + Send + Sync>) -> Self {
        Relayed { shared: Default::default(), host: host.to_owned(), stop }
    }
}

impl Drop for Relayed {
    fn drop(&mut self) {
        self.shared.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        self.shared.tunnel.lock().unwrap_or_else(|e| e.into_inner()).take();
        if !self.shared.ended_there.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(step) = self.shared.step.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                (self.stop)(&self.host, &step);
            }
        }
    }
}

#[cfg(test)]
mod tests;
