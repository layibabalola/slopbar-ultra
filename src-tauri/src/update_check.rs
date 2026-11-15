//! The update check's deadlines, its record and its schedule, apart from Tauri so they can be
//! tested. Every network step has a deadline and every pass of the update loop an outer one, so
//! nothing can stall the loop; each outcome is kept for agents (`get_app_info`'s `update_check`);
//! and a failed pass is tried again sooner than the loop's interval.

use std::future::Future;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

/// The check's request for the channel's small `latest.json`. The updater sets none by itself.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// The installer's download (about 20 MB), enough for a slow link (about 35 KB/s). The updater
/// does not carry the check's deadline over to the download, so the update gets its own.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// A check that Settings or an agent asks for answers by then, whatever happens.
pub const CHECK_LIMIT: Duration = Duration::from_secs(CHECK_TIMEOUT.as_secs() + 10);
/// An install, and each pass of the update loop (which checks and installs), ends by then whatever
/// it waits for: the check, the download and two minutes for the rest.
pub const INSTALL_LIMIT: Duration = Duration::from_secs(CHECK_TIMEOUT.as_secs() + DOWNLOAD_TIMEOUT.as_secs() + 2 * 60);
/// After a failed pass the loop tries again this soon, once; failing again, it keeps its interval.
pub const RETRY_AFTER: Duration = Duration::from_secs(2 * 60);
/// Errors are kept to this many characters.
const MAX_ERROR: usize = 240;

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
    /// Why it failed.
    Failed(String),
}

impl Outcome {
    fn result(&self) -> &'static str {
        match self {
            Outcome::UpToDate => "up_to_date",
            Outcome::Found(_) => "found",
            Outcome::Installing(_) => "installing",
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

/// The last check and when the update loop runs next (`AppState::update_check`).
#[derive(Default)]
pub struct Status(Mutex<Seen>);

#[derive(Default)]
struct Seen {
    last: Option<Check>,
    next_at: Option<DateTime<Utc>>,
}

impl Status {
    fn seen(&self) -> MutexGuard<'_, Seen> {
        // Nothing here can be left half-written, and the update loop must never stop on a panic.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keeps `outcome` as the last check, now. An error is kept fit to show (see `clean`).
    pub fn record(&self, by: Trigger, outcome: Outcome) {
        let outcome = match outcome {
            Outcome::Failed(error) => Outcome::Failed(clean(&error)),
            other => other,
        };
        self.seen().last = Some(Check { at: Utc::now(), by, outcome });
    }

    /// When the update loop runs next (it checks then if automatic updates are on).
    pub fn schedule(&self, next_at: Option<DateTime<Utc>>) {
        self.seen().next_at = next_at;
    }

    #[cfg(test)]
    pub fn last(&self) -> Option<Check> {
        self.seen().last.clone()
    }

    #[cfg(test)]
    pub fn next_at(&self) -> Option<DateTime<Utc>> {
        self.seen().next_at
    }

    /// `get_app_info`'s `update_check`: the last check's time (UTC), result (`up_to_date`, `found`,
    /// `installing` or `error`), version, error and trigger, all null before the first; the update
    /// loop's next run and its interval.
    pub fn report(&self, every: Duration) -> serde_json::Value {
        let seen = self.seen();
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
            "by": last.map(|c| c.by),
            "next_at": seen.next_at.map(stamp),
            "interval_seconds": every.as_secs(),
        })
    }
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// `step`, or an error naming `what` once `limit` has passed without it finishing.
pub async fn bounded<T>(limit: Duration, what: &str, step: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::time::timeout(limit, step)
        .await
        .unwrap_or_else(|_| Err(format!("{what} did not finish within {}", span(limit))))
}

/// The wait after a pass of the update loop, given how many passes before it failed in a row:
/// the interval after a success; `RETRY_AFTER` after a first failure (never longer than the
/// interval); the interval again after more, so a lasting failure is never a tight loop.
pub fn next_wait(failed: bool, failures: &mut u32, every: Duration) -> Duration {
    if !failed {
        *failures = 0;
        return every;
    }
    *failures = failures.saturating_add(1);
    if *failures == 1 { RETRY_AFTER.min(every) } else { every }
}

/// One pass of the update loop: `attempt` (a check, and an install when it finds one) within
/// `limit`, its outcome recorded, and the next pass scheduled. Returns the wait before it.
pub async fn pass(status: &Status, limit: Duration, attempt: impl Future<Output = Result<Outcome, String>>, failures: &mut u32, every: Duration) -> Duration {
    let outcome = bounded(limit, "The automatic update check", attempt).await.unwrap_or_else(Outcome::Failed);
    let wait = next_wait(matches!(outcome, Outcome::Failed(_)), failures, every);
    status.record(Trigger::Auto, outcome);
    status.schedule(after(wait));
    wait
}

/// Now plus `wait`.
pub fn after(wait: Duration) -> Option<DateTime<Utc>> {
    chrono::Duration::from_std(wait).ok().and_then(|d| Utc::now().checked_add_signed(d))
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

/// One line of at most `MAX_ERROR` characters, its URLs cut to scheme, host and path: a signed
/// download link carries its signature in the query, and a URL may carry a user and password.
pub fn clean(text: &str) -> String {
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
    let line = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= MAX_ERROR {
        return line;
    }
    let mut short: String = line.chars().take(MAX_ERROR - 1).collect();
    short.push('…');
    short
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
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Arc;
    use std::time::Instant;

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

    fn failed(status: &Status) -> String {
        match status.last().map(|c| c.outcome) {
            Some(Outcome::Failed(error)) => error,
            other => panic!("expected a failed check, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_check_that_never_answers_fails_in_time_is_recorded_and_retried_soon() {
        let (url, accepted) = silent_server().await;
        let status = Status::default();
        let mut failures = 0;

        // The request's own deadline (the updater's `timeout`) ends it.
        let deadline = Duration::from_millis(300);
        let client = reqwest::Client::builder().no_proxy().timeout(deadline).build().unwrap();
        let started = Instant::now();
        let wait = pass(&status, Duration::from_secs(20), fetch(client, url.clone()), &mut failures, QUARTER_HOUR).await;
        let took = started.elapsed();
        assert!(took >= deadline && took < Duration::from_secs(10), "took {took:?}");
        assert_eq!(accepted.load(SeqCst), 1, "the server took the connection and kept quiet");
        let error = failed(&status);
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("http://127.0.0.1:") && error.contains("/latest.json"), "{error}");
        assert!(!error.contains("Signature") && !error.contains("synthetic-signature") && !error.contains('?'), "{error}");
        assert_eq!(status.last().unwrap().by, Trigger::Auto);
        assert_eq!((wait, failures), (RETRY_AFTER, 1), "a failure is tried again after two minutes");
        let ahead = status.next_at().unwrap() - Utc::now();
        assert!(ahead > chrono::Duration::seconds(110) && ahead <= chrono::Duration::seconds(120), "{ahead}");

        // A request with no deadline of its own: the pass's limit ends it. A second failure in a
        // row goes back to the interval.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let limit = Duration::from_millis(400);
        let started = Instant::now();
        let wait = pass(&status, limit, fetch(client, url), &mut failures, QUARTER_HOUR).await;
        let took = started.elapsed();
        assert!(took >= limit && took < Duration::from_secs(10), "took {took:?}");
        assert_eq!(accepted.load(SeqCst), 2);
        assert_eq!(failed(&status), "The automatic update check did not finish within 400 ms");
        assert_eq!((wait, failures), (QUARTER_HOUR, 2), "no tight loop while it keeps failing");
        let ahead = status.next_at().unwrap() - Utc::now();
        assert!(ahead > chrono::Duration::seconds(890) && ahead <= chrono::Duration::seconds(900), "{ahead}");

        // A pass that works keeps the interval and starts the count again.
        let wait = pass(&status, limit, async { Ok(Outcome::UpToDate) }, &mut failures, QUARTER_HOUR).await;
        assert_eq!((wait, failures), (QUARTER_HOUR, 0));
        assert_eq!(status.last().unwrap().outcome, Outcome::UpToDate);
    }

    #[test]
    fn a_failure_is_retried_once_sooner_then_the_interval_holds() {
        let mut failures = 0;
        let waits: Vec<_> = [true, true, true, false, true, false].iter().map(|&failed| next_wait(failed, &mut failures, QUARTER_HOUR)).collect();
        assert_eq!(waits, [RETRY_AFTER, QUARTER_HOUR, QUARTER_HOUR, QUARTER_HOUR, RETRY_AFTER, QUARTER_HOUR]);
        assert_eq!(failures, 0);
        let six_hours = Duration::from_secs(6 * 60 * 60);
        assert_eq!(next_wait(false, &mut failures, six_hours), six_hours, "the normal interval is unchanged");
        assert_eq!(next_wait(true, &mut failures, six_hours), RETRY_AFTER);
        // Never later than a working machine would check.
        let minute = Duration::from_secs(60);
        assert_eq!(next_wait(true, &mut 0, minute), minute);
    }

    #[test]
    fn deadlines_fit_their_jobs() {
        assert!((30..=60).contains(&CHECK_TIMEOUT.as_secs()));
        // About 20 MB at 35 KB/s.
        assert!(DOWNLOAD_TIMEOUT.as_secs() >= 20 * 1024 / 35);
        assert!(CHECK_LIMIT > CHECK_TIMEOUT);
        assert!(INSTALL_LIMIT > CHECK_TIMEOUT + DOWNLOAD_TIMEOUT);
        assert!(RETRY_AFTER < QUARTER_HOUR);
        assert_eq!(span(INSTALL_LIMIT), "12 min 30 s");
        assert_eq!((span(CHECK_LIMIT), span(DOWNLOAD_TIMEOUT), span(Duration::from_millis(400))), ("40 s".into(), "10 min".into(), "400 ms".into()));
    }

    #[tokio::test]
    async fn bounded_ends_a_step_that_never_finishes() {
        let started = Instant::now();
        let result: Result<(), String> = bounded(Duration::from_millis(50), "The update check", std::future::pending()).await;
        assert_eq!(result.unwrap_err(), "The update check did not finish within 50 ms");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(bounded(Duration::from_secs(5), "x", async { Ok::<_, String>(7) }).await, Ok(7));
        assert_eq!(bounded(Duration::from_secs(5), "x", async { Err::<(), _>("no".to_string()) }).await, Err("no".into()));
    }

    #[test]
    fn the_last_check_is_kept_for_agents() {
        let status = Status::default();
        let every = QUARTER_HOUR;
        assert_eq!(status.report(every), serde_json::json!({ "at": null, "result": null, "version": null, "error": null,
            "by": null, "next_at": null, "interval_seconds": 900 }));

        let before = Utc::now() - chrono::Duration::seconds(1);
        status.record(Trigger::Manual, Outcome::Found("0.1.0-synthetic.7".into()));
        let report = status.report(every);
        assert_eq!((&report["result"], &report["version"], &report["by"]), (&"found".into(), &"0.1.0-synthetic.7".into(), &"manual".into()));
        assert!(report["error"].is_null());
        let at = report["at"].as_str().unwrap();
        assert!(at.ends_with('Z') && at.len() == "2026-11-15T10:00:00Z".len(), "UTC, whole seconds: {at}");
        let at = DateTime::parse_from_rfc3339(at).unwrap();
        assert!(at >= before && at <= Utc::now());

        status.record(Trigger::Auto, Outcome::Failed("error sending request for url (https://objects.example.invalid/release-asset/42?X-Amz-Signature=SYNTHETIC&se=2026)\n  operation timed out".into()));
        let report = status.report(every);
        assert_eq!((&report["result"], &report["by"]), (&"error".into(), &"auto".into()));
        assert_eq!(report["error"], "error sending request for url (https://objects.example.invalid/release-asset/42) operation timed out");
        assert!(report["version"].is_null());

        status.record(Trigger::Auto, Outcome::Installing("0.1.0-synthetic.8".into()));
        let report = status.report(every);
        assert_eq!((&report["result"], &report["version"]), (&"installing".into(), &"0.1.0-synthetic.8".into()));
        assert!(report["error"].is_null());

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
        assert_eq!(long.chars().count(), MAX_ERROR);
        assert!(long.ends_with('…'));
        assert_eq!(clean(&long), long, "cleaning twice changes nothing");
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
