//! The update check's deadlines, its record and its schedule, apart from Tauri so they can be
//! tested.
//!
//! What is bounded, from the inside out:
//! - the check's request: `CHECK_TIMEOUT`, the updater's own deadline;
//! - every request, the download's included: `STALL_TIMEOUT` without data;
//! - the download: `DOWNLOAD_TIMEOUT` in all;
//! - each step of an install (`Steps::run`): a limit of its own, which names the step;
//! - a pass of the update loop, and an install asked for: `PASS_LIMIT`, in a task of its own
//!   (`isolated`), so a panic or the limit ends that pass and nothing else.
//!
//! Not bounded: the installer once it has started, which is a program of its own; and a handover
//! that outlives its limit, whose thread cannot be stopped. That thread keeps the install's turn
//! (one install at a time), so the next install is deferred, or fails once it has waited past
//! `PASS_LIMIT`.

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

/// The check's request for the channel's small `latest.json`, from connecting to the last byte
/// (`UpdaterBuilder::timeout`, which the updater applies to the check only).
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// No data for this long ends a request, the download's included (reqwest's read timeout, through
/// `UpdaterBuilder::configure_client`, which the update the check finds carries to its download).
pub const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The whole download, set on the update itself (the check's deadline does not carry over to it):
/// enough for the installer on a slow link.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The check, as a step: a little past its request's deadline.
pub const CHECK_LIMIT: Duration = Duration::from_secs(40);
/// The download, as a step: a little past its deadline.
pub const DOWNLOAD_LIMIT: Duration = Duration::from_secs(31 * 60);
/// Waiting for a refresh to finish before the installer ends SlopBar (in builds that wait for one).
pub const REFRESH_WAIT_LIMIT: Duration = Duration::from_secs(2 * 60);
/// Handing over to the installer: writing it out and starting it, after the exit cleanup in builds
/// that run it first.
pub const HANDOFF_LIMIT: Duration = Duration::from_secs(60);
/// Each part of SlopBar's exit cleanup that can wait (`app::on_exit`).
pub const EXIT_PART_LIMIT: Duration = Duration::from_secs(15);
/// A pass of the update loop, or an install that Settings or an agent asks for: all its steps, and
/// a minute to spare.
pub const PASS_LIMIT: Duration = Duration::from_secs(
    CHECK_LIMIT.as_secs() + DOWNLOAD_LIMIT.as_secs() + REFRESH_WAIT_LIMIT.as_secs() + HANDOFF_LIMIT.as_secs() + 60,
);
/// After a failed pass the loop tries again this soon, once; failing again, it keeps its interval.
pub const RETRY_AFTER: Duration = Duration::from_secs(2 * 60);
/// Why an install did not start: another one runs.
pub const ALREADY_INSTALLING: &str = "An update is already being installed";
/// Errors and reasons are kept to this many characters.
const MAX_TEXT: usize = 240;

/// Who asked for a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// The update loop.
    Auto,
    /// Settings or an agent (`check_for_updates`, `install_update`).
    Manual,
}

/// What a check, or an install, came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    /// This newer version is available (a check that does not install).
    Found(String),
    /// Handed to this version's installer, which ends this process and starts the new version.
    Installing(String),
    /// Put off, not failed, for this reason: another install runs, or a sign-in is open.
    Deferred(String),
    /// Why it failed.
    Failed(String),
}

impl Outcome {
    fn result(&self) -> &'static str {
        match self {
            Outcome::UpToDate => "up_to_date",
            Outcome::Found(_) => "found",
            Outcome::Installing(_) => "installing",
            Outcome::Deferred(_) => "deferred",
            Outcome::Failed(_) => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub at: DateTime<Utc>,
    pub by: Trigger,
    pub outcome: Outcome,
}

/// A lock that a panic elsewhere does not poison for us: nothing here is ever left half-written,
/// and the update loop must never stop on one.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The last check, when the update loop runs next, and the install running now
/// (`AppState::update_check`).
#[derive(Default)]
pub struct Status {
    seen: Mutex<Seen>,
    /// When the install running now started.
    install: Arc<Mutex<Option<DateTime<Utc>>>>,
}

#[derive(Default)]
struct Seen {
    last: Option<Check>,
    next_at: Option<DateTime<Utc>>,
}

/// An install's turn: the next install can start once it is dropped.
pub struct Turn(Arc<Mutex<Option<DateTime<Utc>>>>);

impl Drop for Turn {
    fn drop(&mut self) {
        *lock(&self.0) = None;
    }
}

impl Status {
    /// Keeps `outcome` as the last check, now. Its text is kept fit to show (see `clean`).
    pub fn record(&self, by: Trigger, outcome: Outcome) {
        let outcome = match outcome {
            Outcome::Failed(error) => Outcome::Failed(clean(&error)),
            Outcome::Deferred(reason) => Outcome::Deferred(clean(&reason)),
            other => other,
        };
        lock(&self.seen).last = Some(Check { at: Utc::now(), by, outcome });
    }

    /// When the update loop runs next (it checks then if automatic updates are on).
    pub fn schedule(&self, next_at: Option<DateTime<Utc>>) {
        lock(&self.seen).next_at = next_at;
    }

    /// Starts an install, one at a time: its turn, or what this attempt comes to (`blocked`).
    pub fn begin_install(&self) -> Result<Turn, Outcome> {
        let mut running = lock(&self.install);
        if let Some(since) = *running {
            return Err(blocked(since));
        }
        *running = Some(Utc::now());
        Ok(Turn(self.install.clone()))
    }

    /// What an install asked for now would come to, while another one runs.
    pub fn install_blocked(&self) -> Option<Outcome> {
        lock(&self.install).map(blocked)
    }

    #[cfg(test)]
    pub fn last(&self) -> Option<Check> {
        lock(&self.seen).last.clone()
    }

    #[cfg(test)]
    pub fn next_at(&self) -> Option<DateTime<Utc>> {
        lock(&self.seen).next_at
    }

    /// `get_app_info`'s `update_check`: the last check's time (UTC), result (`up_to_date`, `found`,
    /// `installing`, `deferred` or `error`), version, error, the reason it was deferred and who
    /// asked, all null before the first; when the update loop runs next, its interval, and the
    /// limit of one of its passes.
    pub fn report(&self, every: Duration) -> serde_json::Value {
        let seen = lock(&self.seen);
        let last = seen.last.as_ref();
        serde_json::json!({
            "at": last.map(|c| stamp(c.at)),
            "result": last.map(|c| c.outcome.result()),
            "version": last.and_then(|c| match &c.outcome {
                Outcome::Found(v) | Outcome::Installing(v) => Some(v.as_str()),
                _ => None,
            }),
            "error": last.and_then(|c| match &c.outcome {
                Outcome::Failed(e) => Some(e.as_str()),
                _ => None,
            }),
            "reason": last.and_then(|c| match &c.outcome {
                Outcome::Deferred(r) => Some(r.as_str()),
                _ => None,
            }),
            "by": last.map(|c| c.by),
            "next_at": seen.next_at.map(stamp),
            "interval_seconds": every.as_secs(),
            "pass_limit_seconds": PASS_LIMIT.as_secs(),
        })
    }
}

/// What an install comes to while another has run since `since`: deferred; or failed once that one
/// has run past the pass limit, since a handover stuck past its limit keeps its turn.
fn blocked(since: DateTime<Utc>) -> Outcome {
    let running = (Utc::now() - since).to_std().unwrap_or_default();
    if running > PASS_LIMIT {
        Outcome::Failed(format!("An install started {} ago has not finished", span(running)))
    } else {
        Outcome::Deferred(ALREADY_INSTALLING.into())
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Now plus `wait`.
pub fn after(wait: Duration) -> Option<DateTime<Utc>> {
    chrono::Duration::from_std(wait).ok().and_then(|d| Utc::now().checked_add_signed(d))
}

/// The step an install is at, so reaching a limit, or a panic, names it.
#[derive(Clone)]
pub struct Steps(Arc<Mutex<&'static str>>);

impl Default for Steps {
    fn default() -> Self {
        Steps(Arc::new(Mutex::new("Starting the update")))
    }
}

impl Steps {
    /// The step running now.
    pub fn current(&self) -> &'static str {
        *lock(&self.0)
    }

    /// `step`, named `name`, within `limit`: its error, or reaching the limit, names it.
    pub async fn run<T>(&self, name: &'static str, limit: Duration, step: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        *lock(&self.0) = name;
        match tokio::time::timeout(limit, step).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(format!("{name} failed: {error}")),
            Err(_) => Err(format!("{name} timed out after {}", span(limit))),
        }
    }
}

/// `attempt` (an install, or a pass of the update loop) in a task of its own within `limit`: a
/// panic, or reaching the limit, fails it with the step it was at, and nothing else.
pub async fn isolated<F, Fut>(limit: Duration, attempt: F) -> Outcome
where
    F: FnOnce(Steps) -> Fut,
    Fut: Future<Output = Result<Outcome, String>> + Send + 'static,
{
    let steps = Steps::default();
    let mut task = tokio::spawn(attempt(steps.clone()));
    match tokio::time::timeout(limit, &mut task).await {
        Ok(Ok(Ok(outcome))) => outcome,
        Ok(Ok(Err(error))) => Outcome::Failed(error),
        Ok(Err(error)) => Outcome::Failed(match error.try_into_panic() {
            Ok(panic) => format!("{} panicked: {}", steps.current(), panic_message(panic.as_ref())),
            Err(_) => format!("{} was cancelled", steps.current()),
        }),
        Err(_) => {
            task.abort();
            Outcome::Failed(format!("The update timed out after {} in: {}", span(limit), steps.current()))
        }
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".into())
}

/// When the update loop runs next: the wait after each pass.
#[derive(Debug, Default)]
pub struct Schedule {
    /// Failed passes in a row.
    failures: u32,
}

impl Schedule {
    /// The wait after a pass that came to `outcome`: the interval after a success; `RETRY_AFTER`
    /// (never longer than the interval) after the first failure in a row, then the interval again,
    /// so a lasting failure is never a tight loop; and the interval after a deferral, which is no
    /// failure and gets no early retry.
    pub fn after(&mut self, outcome: &Outcome, every: Duration) -> Duration {
        match outcome {
            Outcome::Failed(_) => {
                self.failures = self.failures.saturating_add(1);
                if self.failures == 1 { RETRY_AFTER.min(every) } else { every }
            }
            Outcome::Deferred(_) => every,
            _ => {
                self.failures = 0;
                every
            }
        }
    }
}

/// The update loop: a pass `first` after it starts, then one after each wait (`Schedule`). While
/// `auto()` says automatic updates are on, a pass runs `attempt` isolated within `limit` and is
/// recorded in `status`, which also says when the loop runs next. `sleep` is `tokio::time::sleep`;
/// tests pass their own.
pub async fn run_loop<A, AF, S, SF>(status: &Status, first: Duration, every: Duration, limit: Duration, auto: impl Fn() -> bool, attempt: A, sleep: S)
where
    A: Fn(Steps) -> AF,
    AF: Future<Output = Result<Outcome, String>> + Send + 'static,
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    let mut schedule = Schedule::default();
    let mut wait = first;
    loop {
        status.schedule(after(wait));
        sleep(wait).await;
        wait = if auto() {
            let outcome = isolated(limit, &attempt).await;
            let wait = schedule.after(&outcome, every);
            status.record(Trigger::Auto, outcome);
            wait
        } else {
            schedule = Schedule::default();
            every
        };
    }
}

/// Runs `part` on a thread of its own and waits at most `limit` for it (the exit cleanup's parts):
/// whether it finished. A part still running then carries on, and ends with the process.
pub fn wait_at_most(limit: Duration, part: impl FnOnce() + Send + 'static) -> bool {
    let (done, finished) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("bounded-part".into()).spawn(move || {
        part();
        let _ = done.send(());
    });
    spawned.is_ok() && finished.recv_timeout(limit).is_ok()
}

/// An error with its causes, which carry the useful part (a request's "operation timed out"),
/// made fit to show (see `clean`).
pub fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(e) = cause {
        let more = e.to_string();
        if !more.is_empty() && !text.contains(&more) {
            text.push_str(": ");
            text.push_str(&more);
        }
        cause = e.source();
    }
    clean(&text)
}

/// One line of at most `MAX_TEXT` characters with nothing secret or personal in it: URLs cut to
/// scheme, host and path (a signed download link carries its signature in the query, and a URL may
/// carry a user and password), and the user's profile folder, as in a temporary file's path,
/// written `%USERPROFILE%`.
pub fn clean(text: &str) -> String {
    let text = without_profile(&without_queries(text), dirs::home_dir().as_deref());
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= MAX_TEXT {
        return line;
    }
    let mut short: String = line.chars().take(MAX_TEXT - 1).collect();
    short.push('…');
    short
}

fn without_queries(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = [lower.find("https://"), lower.find("http://")].into_iter().flatten().min() else { break };
        // A URL runs to the first character it can never hold unencoded, so no part of it is left.
        let len = rest[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>' | '`' | '{' | '}' | '|' | '\\' | '^'))
            .unwrap_or(rest.len() - start);
        out.push_str(&rest[..start]);
        out.push_str(&place(&rest[start..start + len]));
        rest = &rest[start + len..];
    }
    out.push_str(rest);
    out
}

/// A URL's scheme, host (and port) and path, and the closing punctuation after its query, which
/// belongs to the message around it ("for url (https://…?…): …").
fn place(url: &str) -> String {
    let Some((scheme, tail)) = url.split_once("://") else { return url.into() };
    let (kept, closing) = match tail.find(|c: char| c == '?' || c == '#') {
        Some(cut) => {
            let query = &tail[cut..];
            let unclosed = query.trim_end_matches(|c: char| matches!(c, ')' | ']' | '\'' | '.' | ',' | ';' | ':' | '!'));
            (&tail[..cut], &query[unclosed.len()..])
        }
        None => (tail, ""),
    };
    let (authority, path) = kept.split_once('/').map_or((kept, None), |(a, p)| (a, Some(p)));
    let host = authority.rsplit('@').next().unwrap_or_default();
    match path {
        Some(path) => format!("{scheme}://{host}/{path}{closing}"),
        None => format!("{scheme}://{host}{closing}"),
    }
}

/// `text` with the user's profile folder written `%USERPROFILE%`: `home` itself, and any
/// `<drive>:\Users\<name>`, whatever the separators (a path printed with `{:?}` doubles its
/// backslashes) and however the name is written (`RUNNER~1` is a short name).
fn without_profile(text: &str, home: Option<&Path>) -> String {
    let home: Vec<&str> = home.and_then(Path::to_str).map(|h| h.split(&['\\', '/'][..]).collect()).unwrap_or_default();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        let end = (home.len() > 1).then(|| path_at(text, i, &home)).flatten().or_else(|| users_at(text, i));
        match end {
            Some(end) => {
                out.push_str("%USERPROFILE%");
                i = end;
            }
            None => {
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

fn is_separator(b: u8) -> bool {
    b == b'\\' || b == b'/'
}

/// Where the separators at byte `i` of `text` end, if there are any.
fn separators(text: &str, mut i: usize) -> Option<usize> {
    let start = i;
    while text.as_bytes().get(i).copied().is_some_and(is_separator) {
        i += 1;
    }
    (i > start).then_some(i)
}

/// Where the path made of `parts` ends if it starts at byte `at` of `text`: the parts in order,
/// case aside, with separators between them, up to where a name ends.
fn path_at(text: &str, at: usize, parts: &[&str]) -> Option<usize> {
    let mut i = at;
    for (n, part) in parts.iter().enumerate() {
        if n > 0 {
            i = separators(text, i)?;
        }
        let end = i + part.len();
        if !text.get(i..end)?.eq_ignore_ascii_case(part) {
            return None;
        }
        i = end;
    }
    // A longer name is another folder.
    let name_goes_on = text.as_bytes().get(i).is_some_and(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~') || b >= 0x80);
    (i > at && !name_goes_on).then_some(i)
}

/// Where `<drive>:\Users\<name>` ends if it starts at byte `at` of `text`.
fn users_at(text: &str, at: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if !bytes.get(at)?.is_ascii_alphabetic() || bytes.get(at + 1) != Some(&b':') || (at > 0 && bytes[at - 1].is_ascii_alphanumeric()) {
        return None;
    }
    let users = separators(text, at + 2)?;
    if !text.get(users..users + 5)?.eq_ignore_ascii_case("users") {
        return None;
    }
    let name = separators(text, users + 5)?;
    let mut end = name;
    while bytes.get(end).is_some_and(|&b| !is_separator(b) && !b.is_ascii_whitespace() && !matches!(b, b'"' | b'\'' | b':' | b'*' | b'?' | b'<' | b'>' | b'|')) {
        end += 1;
    }
    (end > name).then_some(end)
}

/// "750 ms", "30 s", "10 min", "12 min 30 s".
fn span(d: Duration) -> String {
    match (d.as_secs() / 60, d.as_secs() % 60) {
        (0, 0) => format!("{} ms", d.as_millis()),
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use std::time::Instant;

    const FIRST: Duration = Duration::from_secs(20);
    const QUARTER_HOUR: Duration = Duration::from_secs(15 * 60);

    /// A server on this machine that accepts connections and never answers. Returns a URL with a
    /// synthetic signature in its query, and how many connections it took.
    async fn silent_server() -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}/latest.json?X-Amz-Signature=synthetic-signature-0042", listener.local_addr().unwrap().port());
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = accepted.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                count.fetch_add(1, SeqCst);
                held.push(socket);
            }
        });
        (url, accepted)
    }

    /// The update check's request, as the updater makes it: GET the channel's JSON.
    async fn fetch(client: reqwest::Client, url: String) -> Result<Outcome, String> {
        let response = client.get(&url).send().await.map_err(|e| describe(&e))?;
        let body = response.text().await.map_err(|e| describe(&e))?;
        Ok(if body.is_empty() { Outcome::UpToDate } else { Outcome::Found(body) })
    }

    /// Waits up to 5 seconds for `check` to hold, without racing the task that makes it so.
    async fn eventually(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// One wait the update loop asked for, with what it had recorded by then.
    #[derive(Debug)]
    struct Waited {
        wait: Duration,
        at: Instant,
        last: Option<Outcome>,
        next_at: Option<DateTime<Utc>>,
    }

    /// Runs the update loop until it asks for its `n`-th wait (the first is `FIRST`, every other
    /// pass waits are its schedule's), with each wait ending at once, and returns what it asked for.
    async fn run_until<A, AF>(status: &Status, n: usize, auto: impl Fn() -> bool, attempt: A) -> Vec<Waited>
    where
        A: Fn(Steps) -> AF,
        AF: Future<Output = Result<Outcome, String>> + Send + 'static,
    {
        let waited = Mutex::new(Vec::new());
        let enough = tokio::sync::Notify::new();
        let sleep = |wait: Duration| {
            let mut waited = waited.lock().unwrap();
            waited.push(Waited { wait, at: Instant::now(), last: status.last().map(|c| c.outcome), next_at: status.next_at() });
            let last = waited.len() >= n;
            if last {
                enough.notify_one();
            }
            async move {
                if last {
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            _ = run_loop(status, FIRST, QUARTER_HOUR, Duration::from_secs(20), auto, attempt, sleep) => unreachable!("the loop never ends"),
            _ = enough.notified() => {}
        }
        waited.into_inner().unwrap()
    }

    fn ahead(at: Option<DateTime<Utc>>, of: Instant, by: Duration) -> bool {
        let Some(at) = at else { return false };
        let expected = Utc::now() - chrono::Duration::from_std(of.elapsed()).unwrap() + chrono::Duration::from_std(by).unwrap();
        (at - expected).num_seconds().abs() <= 2
    }

    #[tokio::test]
    async fn a_check_that_never_answers_fails_in_time_is_recorded_and_retried_soon() {
        let (url, accepted) = silent_server().await;
        let status = Status::default();
        let passes = AtomicUsize::new(0);
        // Pass 1: the request's own deadline (the updater's `timeout`) ends it. Pass 2: a request
        // with none, which the step's limit ends.
        let attempt = |steps: Steps| {
            let first = passes.fetch_add(1, SeqCst) == 0;
            let url = url.clone();
            async move {
                let client = if first { reqwest::Client::builder().timeout(Duration::from_millis(300)) } else { reqwest::Client::builder() };
                let client = client.no_proxy().build().unwrap();
                let limit = if first { Duration::from_secs(10) } else { Duration::from_millis(400) };
                steps.run("The update check", limit, fetch(client, url)).await
            }
        };
        let waited = run_until(&status, 3, || true, attempt).await;
        eventually("the server took both connections and kept quiet", || accepted.load(SeqCst) == 2).await;

        assert_eq!(waited.iter().map(|w| w.wait).collect::<Vec<_>>(), [FIRST, RETRY_AFTER, QUARTER_HOUR]);
        assert!(waited[0].last.is_none() && ahead(waited[0].next_at, waited[0].at, FIRST));

        let took = waited[1].at - waited[0].at;
        assert!(took >= Duration::from_millis(300) && took < Duration::from_secs(10), "pass 1 took {took:?}");
        let Some(Outcome::Failed(error)) = &waited[1].last else { panic!("{:?}", waited[1]) };
        assert!(error.starts_with("The update check failed: ") && error.contains("timed out"), "{error}");
        assert!(error.contains("http://127.0.0.1:") && error.contains("/latest.json"), "{error}");
        assert!(!error.contains("Signature") && !error.contains("synthetic-signature") && !error.contains('?'), "{error}");
        assert!(ahead(waited[1].next_at, waited[1].at, RETRY_AFTER), "the retry is scheduled two minutes on");

        let took = waited[2].at - waited[1].at;
        assert!(took >= Duration::from_millis(400) && took < Duration::from_secs(10), "pass 2 took {took:?}");
        assert_eq!(waited[2].last, Some(Outcome::Failed("The update check timed out after 400 ms".into())));
        assert!(ahead(waited[2].next_at, waited[2].at, QUARTER_HOUR), "failing again, the loop keeps its interval");
        assert_eq!(status.last().unwrap().by, Trigger::Auto);
    }

    #[tokio::test]
    async fn a_panic_ends_its_pass_names_its_step_and_the_loop_goes_on() {
        fn boom() -> Result<Outcome, String> {
            panic!("synthetic panic")
        }
        let status = Status::default();
        let passes = AtomicUsize::new(0);
        let attempt = |steps: Steps| {
            let first = passes.fetch_add(1, SeqCst) == 0;
            async move {
                if first {
                    steps.run("The download", Duration::from_secs(10), async { boom() }).await
                } else {
                    Ok::<_, String>(Outcome::UpToDate)
                }
            }
        };
        let waited = run_until(&status, 3, || true, attempt).await;
        assert_eq!(waited.iter().map(|w| w.wait).collect::<Vec<_>>(), [FIRST, RETRY_AFTER, QUARTER_HOUR]);
        assert_eq!(waited[1].last, Some(Outcome::Failed("The download panicked: synthetic panic".into())));
        assert_eq!(waited[2].last, Some(Outcome::UpToDate));
    }

    #[tokio::test]
    async fn a_pass_past_its_limit_is_stopped_and_says_where() {
        let dropped = Arc::new(AtomicBool::new(false));
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, SeqCst);
            }
        }
        let flag = Flag(dropped.clone());
        let started = Instant::now();
        let outcome = isolated(Duration::from_millis(200), move |steps| async move {
            let _flag = flag;
            steps.run("The download", Duration::from_secs(60), std::future::pending::<Result<(), String>>()).await.map(|()| Outcome::UpToDate)
        })
        .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(outcome, Outcome::Failed("The update timed out after 200 ms in: The download".into()));
        eventually("the pass's task is stopped", || dropped.load(SeqCst)).await;
    }

    #[tokio::test]
    async fn the_loop_keeps_one_schedule_across_passes() {
        let status = Status::default();
        let failed = || Err::<Outcome, String>("synthetic failure".into());
        let script = Mutex::new(VecDeque::from([
            failed(),
            failed(),
            Ok(Outcome::UpToDate),
            Ok(Outcome::Deferred(ALREADY_INSTALLING.into())),
            failed(),
            failed(),
            failed(),
            failed(),
        ]));
        let attempt = |_: Steps| {
            let next = script.lock().unwrap().pop_front().expect("a scripted pass");
            async move { next }
        };
        // Automatic updates go off for one pass (the 7th), which resets the count.
        let asked = AtomicUsize::new(0);
        let auto = || asked.fetch_add(1, SeqCst) != 6;
        let waited = run_until(&status, 10, auto, attempt).await;
        let r = RETRY_AFTER;
        let e = QUARTER_HOUR;
        assert_eq!(waited.iter().map(|w| w.wait).collect::<Vec<_>>(), [FIRST, r, e, e, e, r, e, e, r, e]);
        assert_eq!(waited[4].last, Some(Outcome::Deferred(ALREADY_INSTALLING.into())));
        for w in &waited {
            assert!(ahead(w.next_at, w.at, w.wait), "each wait is scheduled: {w:?}");
        }
    }

    #[test]
    fn a_failure_is_retried_once_sooner_and_a_deferral_waits_the_interval() {
        let fail = Outcome::Failed("x".into());
        let defer = Outcome::Deferred("y".into());
        let mut schedule = Schedule::default();
        let waits: Vec<_> = [&fail, &fail, &fail, &Outcome::UpToDate, &fail, &defer, &fail, &defer, &Outcome::Installing("v".into()), &fail]
            .iter()
            .map(|o| schedule.after(o, QUARTER_HOUR))
            .collect();
        let (r, e) = (RETRY_AFTER, QUARTER_HOUR);
        assert_eq!(waits, [r, e, e, e, r, e, e, e, e, r], "a deferral neither fails nor clears the count");
        let six_hours = Duration::from_secs(6 * 60 * 60);
        assert_eq!(Schedule::default().after(&Outcome::UpToDate, six_hours), six_hours, "the interval is unchanged");
        assert_eq!(Schedule::default().after(&fail, six_hours), RETRY_AFTER);
        let minute = Duration::from_secs(60);
        assert_eq!(Schedule::default().after(&fail, minute), minute, "never later than a working machine checks");
    }

    #[test]
    fn deadlines_nest_inside_their_limits() {
        assert!(CHECK_TIMEOUT < CHECK_LIMIT && CHECK_LIMIT < crate::door::CLIENT_TIMEOUT, "a check asked for answers before the door gives up");
        assert!((30..=60).contains(&CHECK_TIMEOUT.as_secs()));
        assert!(STALL_TIMEOUT < DOWNLOAD_TIMEOUT && DOWNLOAD_TIMEOUT < DOWNLOAD_LIMIT);
        assert!(DOWNLOAD_TIMEOUT >= Duration::from_secs(30 * 60), "slow links");
        assert!(EXIT_PART_LIMIT * 2 + crate::diagnostics::FLUSH_WAIT < HANDOFF_LIMIT, "the handover outlasts the exit cleanup it runs");
        assert!(CHECK_LIMIT + DOWNLOAD_LIMIT + REFRESH_WAIT_LIMIT + HANDOFF_LIMIT < PASS_LIMIT, "a pass outlasts all its steps");
        assert_eq!(RETRY_AFTER, Duration::from_secs(2 * 60), "one retry, after 2 minutes");
        assert_eq!((span(PASS_LIMIT), span(CHECK_LIMIT), span(DOWNLOAD_TIMEOUT)), ("35 min 40 s".into(), "40 s".into(), "30 min".into()));
        assert_eq!(span(Duration::from_millis(400)), "400 ms");
    }

    /// `source` without whole-line comments, so a line commented out does not count.
    fn code(source: &str) -> String {
        source.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
    }

    /// The function in `code` whose signature starts `signature`, to the first line that is `}`.
    fn body<'a>(code: &'a str, signature: &str) -> &'a str {
        let start = code.find(signature).unwrap_or_else(|| panic!("updates.rs has no `{signature}`"));
        let rest = &code[start..];
        rest.find("\n}").map_or(rest, |end| &rest[..end + 2])
    }

    /// updates.rs keeps every deadline and limit wired: no test of the plugin itself can see them.
    #[test]
    fn updates_rs_applies_every_deadline() {
        let code = code(include_str!("updates.rs"));
        let updater = body(&code, "fn updater(");
        assert!(updater.contains(".timeout(update_check::CHECK_TIMEOUT)"), "the check's request deadline, on the builder:\n{updater}");
        assert!(updater.contains(".configure_client(") && updater.contains(".read_timeout(update_check::STALL_TIMEOUT)"), "the stall limit:\n{updater}");
        let install = body(&code, "async fn install_now(");
        assert!(install.contains("update.timeout = Some(update_check::DOWNLOAD_TIMEOUT)"), "the download's deadline, on the update:\n{install}");
        assert!(install.contains(".begin_install()"), "one install at a time");
        for limit in ["update_check::CHECK_LIMIT", "update_check::DOWNLOAD_LIMIT", "update_check::HANDOFF_LIMIT"] {
            assert!(install.contains(limit), "an install's steps run within {limit}");
        }
        assert!(install.contains("spawn_blocking("), "the handover runs on a thread of its own, so its limit can end the wait");
        assert!(body(&code, "pub async fn check(").contains("update_check::CHECK_LIMIT"), "a check asked for has its limit");
        assert!(body(&code, "pub async fn install(").contains("update_check::isolated(update_check::PASS_LIMIT"), "an install asked for runs isolated");
        let looped = body(&code, "pub async fn run_loop(");
        assert!(looped.contains("update_check::run_loop(") && looped.contains("update_check::PASS_LIMIT") && looped.contains("EVERY"), "{looped}");
        assert!(code.contains("const EVERY: Duration = Duration::from_secs(6 * 60 * 60);"), "the interval is unchanged");
    }

    #[test]
    fn one_install_at_a_time() {
        let status = Status::default();
        assert_eq!(status.install_blocked(), None);
        let turn = status.begin_install().ok().expect("the first install starts");
        assert_eq!(status.begin_install().err(), Some(Outcome::Deferred(ALREADY_INSTALLING.into())));
        assert_eq!(status.install_blocked(), Some(Outcome::Deferred(ALREADY_INSTALLING.into())));
        drop(turn);
        assert_eq!(status.install_blocked(), None);
        // A handover stuck past its limit keeps its turn: past the pass limit, that is a failure.
        let stuck = status.begin_install().ok().unwrap();
        *lock(&status.install) = Some(Utc::now() - chrono::Duration::minutes(40));
        assert_eq!(status.begin_install().err(), Some(Outcome::Failed("An install started 40 min ago has not finished".into())));
        drop(stuck);
        assert!(status.begin_install().is_ok());
    }

    #[test]
    fn an_exit_part_that_hangs_is_left_behind_in_time() {
        let started = Instant::now();
        assert!(!wait_at_most(Duration::from_millis(100), || std::thread::sleep(Duration::from_secs(10))));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        assert!(wait_at_most(Duration::from_secs(5), move || flag.store(true, SeqCst)));
        assert!(ran.load(SeqCst));
        // A part that panics only ends its own thread.
        assert!(!wait_at_most(Duration::from_secs(5), || panic!("synthetic panic")));
    }

    #[tokio::test]
    async fn a_step_says_which_step_failed_or_ran_out_of_time() {
        let steps = Steps::default();
        assert_eq!(steps.current(), "Starting the update");
        assert_eq!(steps.run("The download", Duration::from_secs(5), async { Ok::<_, String>(7) }).await, Ok(7));
        assert_eq!(steps.current(), "The download");
        assert_eq!(steps.run("The download", Duration::from_secs(5), async { Err::<(), _>("no".to_string()) }).await, Err("The download failed: no".into()));
        let started = Instant::now();
        let stalled = steps.run("Handing over to the installer", Duration::from_millis(50), std::future::pending::<Result<(), String>>()).await;
        assert_eq!(stalled, Err("Handing over to the installer timed out after 50 ms".into()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn the_last_check_is_kept_for_agents() {
        let status = Status::default();
        let every = QUARTER_HOUR;
        assert_eq!(status.report(every), serde_json::json!({ "at": null, "result": null, "version": null, "error": null, "reason": null,
            "by": null, "next_at": null, "interval_seconds": 900, "pass_limit_seconds": 2140 }));

        let before = Utc::now() - chrono::Duration::seconds(1);
        status.record(Trigger::Manual, Outcome::Found("0.1.0-synthetic.7".into()));
        let report = status.report(every);
        assert_eq!((&report["result"], &report["version"], &report["by"]), (&"found".into(), &"0.1.0-synthetic.7".into(), &"manual".into()));
        assert!(report["error"].is_null() && report["reason"].is_null());
        let at = report["at"].as_str().unwrap();
        assert!(at.ends_with('Z') && at.len() == "2026-11-15T10:00:00Z".len(), "UTC, whole seconds: {at}");
        let at = DateTime::parse_from_rfc3339(at).unwrap();
        assert!(at >= before && at <= Utc::now());

        status.record(Trigger::Auto, Outcome::Failed("The download failed: error sending request for url (https://objects.example.invalid/release-asset/42?X-Amz-Signature=SYNTHETIC&se=2026)\n  operation timed out".into()));
        let report = status.report(every);
        assert_eq!((&report["result"], &report["by"]), (&"error".into(), &"auto".into()));
        assert_eq!(report["error"], "The download failed: error sending request for url (https://objects.example.invalid/release-asset/42) operation timed out");
        assert!(report["version"].is_null() && report["reason"].is_null());

        status.record(Trigger::Manual, Outcome::Deferred(ALREADY_INSTALLING.into()));
        let report = status.report(every);
        assert_eq!((&report["result"], &report["reason"]), (&"deferred".into(), &ALREADY_INSTALLING.into()));
        assert!(report["error"].is_null());

        status.record(Trigger::Auto, Outcome::Installing("0.1.0-synthetic.8".into()));
        let report = status.report(every);
        assert_eq!((&report["result"], &report["version"]), (&"installing".into(), &"0.1.0-synthetic.8".into()));

        status.record(Trigger::Manual, Outcome::UpToDate);
        let next = Utc::now() + chrono::Duration::minutes(15);
        status.schedule(Some(next));
        let report = status.report(every);
        assert_eq!((&report["result"], &report["version"], &report["error"]), (&"up_to_date".into(), &serde_json::Value::Null, &serde_json::Value::Null));
        assert_eq!(report["next_at"], stamp(next));
        status.schedule(None);
        assert!(status.report(every)["next_at"].is_null());
    }

    #[test]
    fn errors_keep_no_secrets_and_stay_short() {
        assert_eq!(clean("GET https://user:pass@host.example.invalid:8443/a/b?token=SECRET#frag failed"), "GET https://host.example.invalid:8443/a/b failed");
        assert_eq!(clean("redirected from HTTPS://Example.invalid?sig=SECRET to http://cdn.example.invalid/x?y=SECRET."),
            "redirected from HTTPS://Example.invalid to http://cdn.example.invalid/x.");
        assert_eq!(clean("(https://a.example.invalid/p?q=SECRET)\r\n\tand  more"), "(https://a.example.invalid/p) and more");
        // Characters a URL may hold before its query do not end it early.
        assert_eq!(clean("see https://h.example.invalid/a(b)/c'd?sig=SECRET). And http://[::1]:8080/x?sig=SECRET"),
            "see https://h.example.invalid/a(b)/c'd). And http://[::1]:8080/x");
        assert_eq!(clean("at <https://h.example.invalid/x?sig=SECRET>|next"), "at <https://h.example.invalid/x>|next");
        assert_eq!(clean("no URL here"), "no URL here");
        let long = clean(&"é".repeat(1000));
        assert_eq!(long.chars().count(), MAX_TEXT);
        assert!(long.ends_with('…'));
        assert_eq!(clean(&long), long, "cleaning twice changes nothing");
    }

    #[test]
    fn a_temporary_files_path_does_not_name_the_user() {
        // A real error for a file in a folder that does not exist under the user's profile, printed
        // as tempfile prints one (`{err} at path {path:?}`, which doubles backslashes on Windows).
        let home = dirs::home_dir().expect("a home folder");
        let path = home.join("AppData").join("Local").join("Temp").join("slopbar-synthetic-missing-7f3a").join("SlopBar-0.1.0-installer.exe");
        let error = std::fs::File::create(&path).unwrap_err();
        let cleaned = clean(&format!("{error} at path {path:?}"));
        let name = home.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(!cleaned.to_lowercase().contains(&name), "{cleaned}");
        assert!(cleaned.contains("%USERPROFILE%") && cleaned.contains("slopbar-synthetic-missing-7f3a"), "{cleaned}");
        // The forms tempfile, Windows and a CI runner's short temp folder print, by hand.
        for (raw, want) in [
            (r#"failed to create temporary file at path "C:\\Users\\synthetic-user\\AppData\\Local\\Temp\\SlopBar-0.1.0-updater-Ab3dEf\\x.exe""#,
             r#"failed to create temporary file at path "%USERPROFILE%\\AppData\\Local\\Temp\\SlopBar-0.1.0-updater-Ab3dEf\\x.exe""#),
            (r"Access is denied. (os error 5) C:\Users\RUNNER~1\AppData\Local\Temp\x", r"Access is denied. (os error 5) %USERPROFILE%\AppData\Local\Temp\x"),
            ("c:/users/Synthetic.User/AppData", "%USERPROFILE%/AppData"),
            (r"no profile in D:\Data\Users or XC:\Users\x", r"no profile in D:\Data\Users or XC:\Users\x"),
        ] {
            assert_eq!(clean(raw), want);
        }
        assert_eq!(without_profile(r"C:\Users\Synthetic Name\AppData and C:\Users\Synthetic Names", Some(Path::new(r"C:\Users\Synthetic Name"))),
            r"%USERPROFILE%\AppData and %USERPROFILE% Names", "the user's own folder, spaces and all; another one by its first word");
        assert_eq!(without_profile("/home/synthetic/.cache and /home/synthetic2", Some(Path::new("/home/synthetic"))), "%USERPROFILE%/.cache and /home/synthetic2");
    }

    #[test]
    fn an_error_is_described_with_its_causes() {
        #[derive(Debug)]
        struct Layer(&'static str, Option<Box<Layer>>);
        impl std::fmt::Display for Layer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.0) }
        }
        impl std::error::Error for Layer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { self.1.as_deref().map(|e| e as _) }
        }
        let error = Layer("error sending request for url (https://example.invalid/latest.json?sig=SECRET)",
            Some(Box::new(Layer("operation timed out", Some(Box::new(Layer("timed out", None)))))));
        assert_eq!(describe(&error), "error sending request for url (https://example.invalid/latest.json): operation timed out");
    }
}
