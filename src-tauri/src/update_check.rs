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
//! that outlives its limit, whose thread cannot be stopped. That thread keeps what the handover
//! holds (`hand_over`): the install's turn, so the next install is deferred, or fails once it has
//! waited past `PASS_LIMIT`; and in builds that hold one, the refresh lock, so a handover that
//! never returns keeps refreshes waiting until SlopBar restarts.

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

/// An install's turn, from its download to the end of its handover: the next install can start
/// once it is dropped. A check does not take one.
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

/// Hands over to the installer: `install` (synchronous, and on Windows it ends this process) on a
/// thread of its own, as a step within `limit`. `held` (the install's turn, and in builds that wait
/// for one, the refresh lock) goes with that thread and is released only when the thread ends, so
/// reaching the limit, which ends the wait, never frees what the installer still needs: a handover
/// that never returns keeps it until SlopBar restarts.
pub async fn hand_over<H: Send + 'static>(
    steps: &Steps,
    limit: Duration,
    held: H,
    install: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    steps
        .run("Handing over to the installer", limit, async move {
            tokio::task::spawn_blocking(move || {
                let _held = held;
                install()
            })
            .await
            .map_err(|e| describe(&e))?
        })
        .await
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
    use super::guard::{calls, find, function, has, lost, tokens, update, words};
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
    use std::time::Instant;

    const FIRST: Duration = Duration::from_secs(20);
    const QUARTER_HOUR: Duration = Duration::from_secs(15 * 60);
    /// How long a test waits for what should happen at once (a part that only sets a flag, a lock
    /// let go). The wait ends as soon as it happens, so only a starved run waits at all, and none this
    /// long: no bound on elapsed time is tighter than this.
    const PATIENT: Duration = Duration::from_secs(5 * 60);
    /// What the tests stall on ends by itself after this. A limit in the code ends it far sooner,
    /// however starved the run (tokio fires timers in deadline order); with the limit gone, it ends
    /// here and the test fails, rather than hanging.
    const SELF_ENDING: Duration = Duration::from_secs(60);
    /// A last backstop for each test that waits on a limit in the code.
    const TEST_LIMIT: Duration = Duration::from_secs(5 * 60);

    /// `test` within TEST_LIMIT.
    async fn within_test_limit<T>(test: impl Future<Output = T>) -> T {
        tokio::time::timeout(TEST_LIMIT, test).await.expect("the test ran past its limit: a limit in the code is missing")
    }

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

    /// Waits up to PATIENT for `check` to hold, without racing the task that makes it so.
    async fn eventually(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + PATIENT;
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
            _ = run_loop(status, FIRST, QUARTER_HOUR, SELF_ENDING, auto, attempt, sleep) => unreachable!("the loop never ends"),
            _ = enough.notified() => {}
        }
        waited.into_inner().unwrap()
    }

    /// Whether `at` is `by` after the moment `of` was taken, within 45 s either way: the waits differ
    /// by 100 s or more, and a starved run has room.
    fn ahead(at: Option<DateTime<Utc>>, of: Instant, by: Duration) -> bool {
        let Some(at) = at else { return false };
        let expected = Utc::now() - chrono::Duration::from_std(of.elapsed()).unwrap() + chrono::Duration::from_std(by).unwrap();
        (at - expected).num_seconds().abs() <= 45
    }

    #[tokio::test]
    async fn a_check_that_never_answers_fails_in_time_is_recorded_and_retried_soon() {
        within_test_limit(async {
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
            assert!(took >= Duration::from_millis(300), "pass 1 took {took:?}");
            let Some(Outcome::Failed(error)) = &waited[1].last else { panic!("{:?}", waited[1]) };
            assert!(error.starts_with("The update check failed: ") && error.contains("timed out"), "{error}");
            assert!(error.contains("http://127.0.0.1:") && error.contains("/latest.json"), "{error}");
            assert!(!error.contains("Signature") && !error.contains("synthetic-signature") && !error.contains('?'), "{error}");
            assert!(ahead(waited[1].next_at, waited[1].at, RETRY_AFTER), "the retry is scheduled two minutes on");

            let took = waited[2].at - waited[1].at;
            assert!(took >= Duration::from_millis(400), "pass 2 took {took:?}");
            assert_eq!(waited[2].last, Some(Outcome::Failed("The update check timed out after 400 ms".into())));
            assert!(ahead(waited[2].next_at, waited[2].at, QUARTER_HOUR), "failing again, the loop keeps its interval");
            assert_eq!(status.last().unwrap().by, Trigger::Auto);
        })
        .await
    }

    #[tokio::test]
    async fn a_panic_ends_its_pass_names_its_step_and_the_loop_goes_on() {
        fn boom() -> Result<Outcome, String> {
            panic!("synthetic panic")
        }
        within_test_limit(async {
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
        })
        .await
    }

    #[tokio::test]
    async fn a_pass_past_its_limit_is_stopped_and_says_where() {
        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, SeqCst);
            }
        }
        within_test_limit(async {
            let dropped = Arc::new(AtomicBool::new(false));
            let finished = Arc::new(AtomicBool::new(false));
            let flag = Flag(dropped.clone());
            let done = finished.clone();
            let outcome = isolated(Duration::from_millis(200), move |steps| async move {
                let _flag = flag;
                let stalled = async {
                    tokio::time::sleep(SELF_ENDING).await;
                    done.store(true, SeqCst);
                    Ok::<_, String>(())
                };
                steps.run("The download", PATIENT, stalled).await.map(|()| Outcome::UpToDate)
            })
            .await;
            assert_eq!(outcome, Outcome::Failed("The update timed out after 200 ms in: The download".into()));
            eventually("the pass's task is stopped", || dropped.load(SeqCst)).await;
            // A task left running would also end, once its stalled step ended by itself; one that
            // was stopped never gets past that step.
            assert!(!finished.load(SeqCst), "the pass's task ran on past its limit until its step ended by itself");
        })
        .await
    }

    #[tokio::test]
    async fn the_loop_keeps_one_schedule_across_passes() {
        within_test_limit(async {
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
        })
        .await
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

    /// Where each guard's needles are, for its failure (see `guard::update`).
    const WIRING: &str = "update_check::tests::updates_rs_wires_each_deadline_to_its_own_step (src-tauri/src/update_check.rs)";
    const TURN: &str = "update_check::tests::only_an_install_takes_the_turn (src-tauri/src/update_check.rs)";
    const EXIT: &str = "update_check::tests::the_exit_cleanup_and_the_door_keep_their_limits (src-tauri/src/update_check.rs)";

    /// Each step (`.run("<name>", <limit>, ..)`) in `code` (tokens): its name, its limit and the
    /// whole call.
    fn steps_in(code: &str) -> Vec<(&str, &str, &str)> {
        calls(code, ".run")
            .into_iter()
            .filter(|call| call.starts_with("(\""))
            .map(|call| {
                let name = call[2..].split('"').next().unwrap();
                let rest = call[2 + name.len() + 1..].trim_start_matches(|c: char| c == ',' || c.is_whitespace());
                (name, rest.split(',').next().unwrap().trim(), call)
            })
            .collect()
    }

    /// updates.rs applies every deadline where it belongs, which no test of the plugin itself can
    /// see: the limit each step runs within and the work it wraps, where the turn is taken, and
    /// that what the handover holds is bound once and handed to it, never let go before.
    #[test]
    fn updates_rs_wires_each_deadline_to_its_own_step() {
        let code = tokens(include_str!("updates.rs"));
        let updater = function(&code, "updates.rs", "fn updater(", WIRING);
        // The check's request deadline, on the builder, and the stall limit.
        for needle in [".timeout(update_check::CHECK_TIMEOUT)", ".configure_client(|client| client.read_timeout(update_check::STALL_TIMEOUT))"] {
            assert!(has(updater, needle), "{}\n{updater}", lost("updater() in updates.rs", needle, WIRING));
        }

        let expected = [
            ("The update check", "update_check::CHECK_LIMIT", "updater(app)?.check()"),
            ("The download", "update_check::DOWNLOAD_LIMIT", "update.download("),
            ("Waiting for a refresh to finish", "update_check::REFRESH_WAIT_LIMIT", "refresh_lock"),
        ];
        let steps = steps_in(&code);
        for (name, limit, call) in &steps {
            let (_, want, work) = expected.iter().find(|(n, ..)| n == name)
                .unwrap_or_else(|| panic!("updates.rs has a step this guard does not know: \"{name}\". {}", update(WIRING)));
            assert_eq!(limit, want, "\"{name}\" runs within {want}. {}", update(WIRING));
            assert!(has(call, work), "{}\n{call}", lost(&format!("The step \"{name}\" in updates.rs"), work, WIRING));
        }
        assert_eq!(steps.iter().filter(|(n, ..)| *n == "The update check").count(), 2, "check() and install_now each check as a step. {}", update(WIRING));
        assert_eq!(steps.iter().filter(|(n, ..)| *n == "The download").count(), 1, "install_now downloads as a step. {}", update(WIRING));

        let install = function(&code, "updates.rs", "async fn install_now(", WIRING);
        let at = |needle: &str| find(install, needle).unwrap_or_else(|| panic!("{}\n{install}", lost("install_now in updates.rs", needle, WIRING)));
        let turn = at("let turn = match app.state::<AppState>().update_check.begin_install()");
        assert!(at("\"The update check\"") < turn && turn < at("\"The download\""), "a check holds no turn; the download does. {}\n{install}", update(WIRING));
        assert!(at("update.timeout = Some(update_check::DOWNLOAD_TIMEOUT)") < at("\"The download\""),
            "the download's deadline is on the update before it downloads. {}\n{install}", update(WIRING));
        let handovers = calls(install, "update_check::hand_over");
        assert_eq!(handovers.len(), 1, "one handover. {}\n{install}", update(WIRING));
        let handover = handovers[0];
        let within = "(steps, update_check::HANDOFF_LIMIT, ";
        assert_eq!(find(handover, within), Some(0), "the handover runs within HANDOFF_LIMIT. {}\n{handover}", lost("The handover in updates.rs", within, WIRING));
        assert!(has(handover, "update.install(bytes)"), "{}\n{handover}", lost("The handover in updates.rs", "update.install(bytes)", WIRING));
        let after = &install[turn..];
        let after = &after[find(after, "};").unwrap_or_else(|| panic!("{}", lost("The turn's binding in updates.rs", "};", WIRING))) + 2..];
        assert_eq!((words(after, "turn"), words(handover, "turn")), (1, 1), "once bound, the turn is used only to hand it to the handover. {}\n{install}", update(WIRING));

        let manual = calls(function(&code, "updates.rs", "pub async fn install(", WIRING), "update_check::isolated");
        assert!(manual.len() == 1 && find(manual[0], "(update_check::PASS_LIMIT, ") == Some(0) && has(manual[0], "install_now("),
            "an install asked for runs isolated within PASS_LIMIT. {}\n{manual:?}", update(WIRING));
        let looped = calls(function(&code, "updates.rs", "pub async fn run_loop(", WIRING), "update_check::run_loop");
        assert!(looped.len() == 1 && find(looped[0], "(&state.update_check, FIRST_CHECK, EVERY, update_check::PASS_LIMIT, ") == Some(0),
            "the update loop runs each pass within PASS_LIMIT, every EVERY. {}\n{looped:?}", update(WIRING));
        let interval = "const EVERY: Duration = Duration::from_secs(6 * 60 * 60);";
        assert!(has(&code, interval), "the interval is unchanged. {}", lost("updates.rs", interval, WIRING));
    }

    /// Only an install takes the turn. A check that took it as well (one that Settings or an agent
    /// asks for) would put off every install that starts while it runs, for up to CHECK_LIMIT.
    #[test]
    fn only_an_install_takes_the_turn() {
        let code = tokens(include_str!("updates.rs"));
        let install = function(&code, "updates.rs", "async fn install_now(", TURN);
        assert_eq!((words(&code, "begin_install"), words(install, "begin_install")), (1, 1),
            "updates.rs takes the install's turn (begin_install) once, in install_now. {}", update(TURN));
    }

    /// The exit cleanup bounds each part that can wait, and the door answers at once while an
    /// install runs (app.rs).
    #[test]
    fn the_exit_cleanup_and_the_door_keep_their_limits() {
        let app = tokens(include_str!("app.rs"));
        let exit = function(&app, "app.rs", "pub(crate) fn on_exit(", EXIT);
        assert!(!has(exit, ".lock().unwrap()"), "a poisoned lock cannot panic the exit. {}\n{exit}", update(EXIT));
        let bounded = calls(exit, "wait_at_most");
        assert!(bounded.len() == 2 && bounded.iter().all(|c| find(c, "(EXIT_PART_LIMIT, ") == Some(0)),
            "each part of the exit cleanup that can wait runs within wait_at_most(EXIT_PART_LIMIT, ..). {}\n{bounded:?}", update(EXIT));
        for part in ["presence::withdraw(", "settings.lock()", "signin::close_all"] {
            let needle = tokens(part);
            let inside: usize = bounded.iter().map(|c| c.matches(&needle).count()).sum();
            assert!(inside == 1 && exit.matches(&needle).count() == 1, "`{part}` runs within wait_at_most(EXIT_PART_LIMIT, ..). {}\n{exit}", update(EXIT));
        }
        // The journal waits FLUSH_WAIT at most.
        for needle in [".settings.lock().unwrap_or_else(|e| e.into_inner())", "diagnostics.record(None, None, diagnostics::Event::Shutdown)"] {
            assert!(has(exit, needle), "{}\n{exit}", lost("on_exit in app.rs", needle, EXIT));
        }

        let arm = function(&app, "app.rs", "R::InstallUpdate => {", EXIT);
        let at = |needle: &str| find(arm, needle).unwrap_or_else(|| panic!("{}\n{arm}", lost("The door's install_update in app.rs", needle, EXIT)));
        assert!(at("if let Some(reason) = updates::install_blocked(&app) {") < at("check_for_updates("),
            "install_update answers at once while an install runs, before it checks. {}\n{arm}", update(EXIT));
    }

    /// The guards read tokens: what rustfmt or a hand changes in layout reads the same; what the
    /// code is does not.
    #[test]
    fn guards_read_tokens_not_layout() {
        let one_line = "update_check::hand_over(steps, update_check::HANDOFF_LIMIT, (turn, refresh), move || update.install(bytes).map_err(|e| describe(&e))).await?;";
        let rustfmt = "update_check::hand_over(\n    steps,\n    update_check::HANDOFF_LIMIT,\n    (turn, refresh),\n    move || update.install(bytes).map_err(|e| describe(&e)),\n)\n.await?;";
        assert_eq!(tokens(rustfmt), tokens(one_line), "a call broken over lines, with rustfmt's trailing comma");
        assert_eq!(tokens("updater(app)?\n            .check()\n            .await"), tokens("updater(app)?.check().await"), "a method chain broken over lines");
        let deferral = "if crate::signin::any_active() {\n        return Ok(Outcome::Deferred(SIGN_IN_OPEN.into()));\n    }";
        assert_eq!(tokens(&deferral.replace('\n', "\n    ")), tokens(deferral), "indented anew");
        assert_eq!(tokens("if  crate::signin::any_active()\n{ return Ok(Outcome::Deferred(SIGN_IN_OPEN.into())); }"), tokens(deferral), "braces and spaces moved");
        assert_eq!(tokens("let  turn =\n match x /* a /* nested */ note */ { // the turn\n Ok(t) => t, }"), "let turn=match x{Ok(t)=>t}", "comments go; words stay apart");
        assert_ne!(tokens("letturn = x"), tokens("let turn = x"), "words are not run together");
        assert_ne!(tokens("(a, b)"), tokens("(a, c)"), "a rename is a change");
        // Literals are kept as written, with what looks like a comment or a bracket inside them.
        let literals = r##"f("a  //  b ) {", '}', '\'', '"', r#"x ) "y" // z"#, b"(", &'a str, 'label: loop {})"##;
        assert_eq!(tokens(literals), r##"f("a  //  b ) {",'}','\'','"',r#"x ) "y" // z"#,b"(",&'a str,'label:loop{})"##);
        assert_eq!(calls(&tokens(literals), "f"), [tokens(literals)[1..].to_string()], "the call ends at its own bracket, past every literal");
        assert_eq!(tokens("a / *b"), "a/ *b", "no comment is made where there was none");
        for source in [include_str!("updates.rs"), include_str!("app.rs"), include_str!("update_check.rs")] {
            let once = tokens(source);
            assert_eq!(tokens(&once), once, "reading tokens again changes nothing");
        }
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
        let failed = status.begin_install().err();
        assert!(matches!(&failed, Some(Outcome::Failed(text)) if text.starts_with("An install started 40 min") && text.ends_with(" ago has not finished")), "{failed:?}");
        drop(stuck);
        assert!(status.begin_install().is_ok());
    }

    #[tokio::test]
    async fn a_handover_past_its_limit_keeps_what_it_holds_until_its_thread_ends() {
        within_test_limit(async {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            let held = lock.clone().lock_owned().await;
            let (finish, installing) = std::sync::mpsc::channel::<()>();
            let steps = Steps::default();
            let result = hand_over(&steps, Duration::from_millis(100), held, move || {
                let _ = installing.recv_timeout(SELF_ENDING);
                Ok(())
            })
            .await;
            assert_eq!(result, Err("Handing over to the installer timed out after 100 ms".into()));
            assert!(lock.try_lock().is_err(), "still held while the handover's thread runs");
            drop(finish);
            eventually("let go once that thread ends", || lock.try_lock().is_ok()).await;
            // In time, it returns the installer's result, and lets go.
            let held = lock.clone().lock_owned().await;
            assert_eq!(hand_over(&steps, PATIENT, held, || Err("synthetic failure".into())).await, Err("Handing over to the installer failed: synthetic failure".into()));
            eventually("let go", || lock.try_lock().is_ok()).await;
        })
        .await
    }

    #[test]
    fn an_exit_part_that_hangs_is_left_behind_in_time() {
        // A part that hangs (it ends by itself only after SELF_ENDING): wait_at_most gives up at its
        // limit. Were the limit gone, it would wait for the part and return true: a failure, not a
        // hang.
        assert!(!wait_at_most(Duration::from_millis(100), || std::thread::sleep(SELF_ENDING)), "a part that hangs is left behind at its limit");
        // A part that finishes is waited for, however long a starved run takes to schedule it.
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        assert!(wait_at_most(PATIENT, move || flag.store(true, SeqCst)));
        assert!(ran.load(SeqCst));
        // A part that panics only ends its own thread, and is not waited for.
        assert!(!wait_at_most(PATIENT, || panic!("synthetic panic")));
    }

    #[tokio::test]
    async fn a_step_says_which_step_failed_or_ran_out_of_time() {
        within_test_limit(async {
            let steps = Steps::default();
            assert_eq!(steps.current(), "Starting the update");
            assert_eq!(steps.run("The download", PATIENT, async { Ok::<_, String>(7) }).await, Ok(7));
            assert_eq!(steps.current(), "The download");
            assert_eq!(steps.run("The download", PATIENT, async { Err::<(), _>("no".to_string()) }).await, Err("The download failed: no".into()));
            let stalls = async {
                tokio::time::sleep(SELF_ENDING).await;
                Ok::<(), String>(())
            };
            let stalled = steps.run("Handing over to the installer", Duration::from_millis(50), stalls).await;
            assert_eq!(stalled, Err("Handing over to the installer timed out after 50 ms".into()));
        })
        .await
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

    /// The deadlines in updates.rs rely on tauri-plugin-updater 2.12.0's internals (its updater.rs:
    /// the builder's `timeout` reaching only the check; `check()` carrying the `configure_client`
    /// hook into the `Update` it returns, with no timeout of its own; `Update::download` applying
    /// both). Every build with updates uses them, so this lives here, where every tree has it. A
    /// different version must be read again before this test is changed.
    #[test]
    fn the_updater_plugin_is_the_version_reviewed() {
        let version = include_str!("../Cargo.lock")
            .split("[[package]]")
            .find(|package| package.lines().any(|l| l == "name = \"tauri-plugin-updater\""))
            .and_then(|package| package.lines().find_map(|l| l.strip_prefix("version = \"")?.strip_suffix('"')));
        assert_eq!(version, Some("2.12.0"), "re-review the deadlines in src-tauri/src/updates.rs against this tauri-plugin-updater");
    }
}

/// Source guards: tests that read code for what no other test can see (the limit each step of an
/// update runs within, where an install takes its turn, what the exit cleanup bounds). They read
/// tokens, not layout (`tokens`), so rustfmt, a call broken over lines or a block indented anew
/// reads the same. A rename does not, and then the failure names the text that went and the test
/// to update (`lost`, `update`).
#[cfg(test)]
pub(crate) mod guard {
    fn word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// `source` as a guard reads it, code and needles alike: no comments; whitespace only where it
    /// keeps two words apart (or `/` from a `/` or `*` after it), as one space; and no comma right
    /// before a closing bracket, which rustfmt adds when it breaks a call over lines. String and
    /// character literals are kept as written.
    pub fn tokens(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut gap = false;
        let mut i = 0;
        while let Some(c) = source[i..].chars().next() {
            let rest = &source[i..];
            if rest.starts_with("//") {
                i += rest.find('\n').unwrap_or(rest.len());
                gap = true;
            } else if rest.starts_with("/*") {
                i += block_comment(rest);
                gap = true;
            } else if c.is_whitespace() {
                i += c.len_utf8();
                gap = true;
            } else {
                let end = literal_end(source, i).unwrap_or(i + c.len_utf8());
                if matches!(c, ')' | ']' | '}') && out.ends_with(',') {
                    out.pop();
                }
                let last = out.chars().next_back();
                if gap && ((word(c) && last.is_some_and(word)) || (last == Some('/') && matches!(c, '/' | '*'))) {
                    out.push(' ');
                }
                out.push_str(&source[i..end]);
                gap = false;
                i = end;
            }
        }
        out
    }

    /// How long the block comment that `rest` starts with is, the comments nested in it included.
    fn block_comment(rest: &str) -> usize {
        let mut depth = 0usize;
        let mut i = 0;
        while let Some(c) = rest[i..].chars().next() {
            if rest[i..].starts_with("/*") {
                depth += 1;
                i += 2;
            } else if rest[i..].starts_with("*/") {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return i;
                }
            } else {
                i += c.len_utf8();
            }
        }
        rest.len()
    }

    /// Where the string or character literal that starts at byte `at` of `s` ends, if one starts
    /// there: `"…"`, `r#"…"#` (and `b`, `c` before either), `'…'`. A lifetime or a label is none.
    fn literal_end(s: &str, at: usize) -> Option<usize> {
        let rest = &s[at..];
        if rest.starts_with('"') {
            let mut escaped = false;
            for (i, c) in rest.char_indices().skip(1) {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => return Some(at + i + 1),
                    _ => {}
                }
            }
            return Some(s.len());
        }
        if rest.starts_with('r') && raw_prefix(s, at) {
            let hashes = rest[1..].len() - rest[1..].trim_start_matches('#').len();
            let open = 1 + hashes;
            if !rest[open..].starts_with('"') {
                return None;
            }
            let close = format!("\"{}", "#".repeat(hashes));
            return Some(rest[open + 1..].find(&close).map_or(s.len(), |e| at + open + 1 + e + close.len()));
        }
        if rest.starts_with('\'') {
            let mut chars = rest.char_indices().skip(1);
            let (_, first) = chars.next()?;
            if first == '\\' {
                chars.next()?;
                return chars.find(|&(_, c)| c == '\'').map(|(i, _)| at + i + 1);
            }
            let (i, second) = chars.next()?;
            return (first != '\'' && second == '\'').then_some(at + i + 1);
        }
        None
    }

    /// Whether an `r` at byte `at` of `s` can start a raw string: it starts a token, alone or after
    /// a `b` or `c` that does.
    fn raw_prefix(s: &str, at: usize) -> bool {
        let mut before = s[..at].chars().rev();
        match before.next() {
            None => true,
            Some('b' | 'c') => !before.next().is_some_and(word),
            Some(c) => !word(c),
        }
    }

    /// The characters of `code` from byte `from` on that are not inside a literal, with their bytes.
    fn outside_literals(code: &str, from: usize) -> impl Iterator<Item = (usize, char)> + '_ {
        let mut i = from;
        std::iter::from_fn(move || loop {
            let c = code[i..].chars().next()?;
            if let Some(end) = literal_end(code, i) {
                i = end;
                continue;
            }
            i += c.len_utf8();
            return Some((i - c.len_utf8(), c));
        })
    }

    /// The bracket that closes the one at byte `open` of `code`.
    fn closing(code: &str, open: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (at, c) in outside_literals(code, open) {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(at);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Where `needle` starts in `code` (tokens), read as tokens itself.
    pub fn find(code: &str, needle: &str) -> Option<usize> {
        code.find(&tokens(needle))
    }

    /// Whether `code` (tokens) has `needle`, read as tokens itself.
    pub fn has(code: &str, needle: &str) -> bool {
        find(code, needle).is_some()
    }

    /// The function (or arm, or block) whose code starts `signature`, through the brace that closes
    /// it; a failure that says what to update in `test` when `file` (tokens: `code`) has none.
    pub fn function<'a>(code: &'a str, file: &str, signature: &str, test: &str) -> &'a str {
        let start = find(code, signature).unwrap_or_else(|| panic!("{}", lost(file, signature, test)));
        let open = outside_literals(code, start).find(|&(_, c)| c == '{').map(|(at, _)| at);
        let close = open.and_then(|open| closing(code, open)).unwrap_or_else(|| panic!("`{signature}` in {file} has no body that closes"));
        &code[start..=close]
    }

    /// Every call of `name` in `code` (tokens), each from its opening parenthesis to its closing one.
    pub fn calls<'a>(code: &'a str, name: &str) -> Vec<&'a str> {
        let name = tokens(name);
        let named = |at: usize| name.starts_with(word) && code[..at].chars().next_back().is_some_and(word);
        code.match_indices(name.as_str())
            .map(|(at, _)| at + name.len())
            .filter(|&open| code[open..].starts_with('(') && !named(open - name.len()))
            .map(|open| &code[open..=closing(code, open).unwrap_or_else(|| panic!("the call of `{name}` at byte {open} never closes"))])
            .collect()
    }

    /// How often `name` stands alone in `code`, not inside a longer name.
    pub fn words(code: &str, name: &str) -> usize {
        let named = |c: Option<char>| c.is_some_and(word);
        code.match_indices(name).filter(|(at, _)| !named(code[..*at].chars().next_back()) && !named(code[at + name.len()..].chars().next())).count()
    }

    /// A guard's failure when `place` no longer has `needle`.
    pub fn lost(place: &str, needle: &str, test: &str) -> String {
        format!("{place} no longer has `{needle}`. {}", update(test))
    }

    /// What every guard's failure ends with: what to update after a rename that keeps the behaviour.
    pub fn update(test: &str) -> String {
        format!("If a rename or a move left what the code does as it was, update the text this guard looks for in {test}. Otherwise the guard caught a change in what the code does.")
    }
}
