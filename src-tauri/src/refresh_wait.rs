//! Waiting for the refresh lock (`AppState::refresh_lock`), always within a limit, and what a
//! refresh does under it for those that must not wait for it.
//!
//! A refresh holds the lock while it runs. In builds that wait for one before an update's
//! installer ends SlopBar, the handover keeps the lock until its thread ends
//! (`update_check::hand_over`), which can be when SlopBar restarts. A lock held that long may leave
//! the numbers stale, and nothing else:
//! - a refresh that someone asks for waits for the lock `LOCK_WAIT` at most (`under`). Then it
//!   gives up and its caller gets the last numbers (`app::do_refresh`), and a refresh follows once
//!   the lock is free (`app::refresh_soon`: one task however many ask (`Coalesce`), which waits
//!   holding nothing and which nobody waits for);
//! - nothing waits for the lock while it holds what others need. A settings save lets go of
//!   `settings_lock` before its refresh, and leaves what it moved to that refresh
//!   (`app::store_settings`, `sharing`); an account's reservation ends before any refresh
//!   (`app::disconnect_one`, `app::start_add_account`); and changes to the usage cache are left to
//!   the next refresh, which makes them under the lock (`CacheDue`), instead of waiting for it;
//! - a door call whose refresh gave up says so in its answer (`noted`), a switch's
//!   `switched_to_requested` is then unknown (`switch_held`), and the door lets a connection go once
//!   its client has given up (`door::CLIENT_TIMEOUT`), so its slots stay free for get_app_info,
//!   restart_app, install_update and quit_app.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::accounts::Provider;
use crate::model::Snapshot;

/// How long anything that answers someone waits for the refresh lock. A refresh normally ends well
/// inside it, and a door call that waits this long and then makes one request of its own (20 s at
/// most, `app::http_client`) still answers before its client gives up (`door::CLIENT_TIMEOUT`).
pub const LOCK_WAIT: Duration = Duration::from_secs(30);

/// The field a door answer gets when a refresh it needed gave up waiting (`noted`)...
pub const WAITING: &str = "waiting_for_refresh";
/// ...and what it says.
pub const WAITING_NOTE: &str = "SlopBar is still busy with an earlier refresh or an update, so these are the last numbers it has. \
A new refresh runs as soon as that one ends: check get_usage shortly.";

tokio::task_local! {
    /// Whether a refresh gave up waiting during the door call this task serves (`noting_waits`).
    static WAITED: Cell<bool>;
}

/// `refresh` under `lock`, once the lock comes within `limit`. Otherwise None: `refresh` never
/// runs, the lock stays with whoever holds it, and the door call this serves hears so
/// (`noting_waits`).
pub async fn under<T, F>(lock: &tokio::sync::Mutex<()>, limit: Duration, refresh: impl FnOnce() -> F) -> Option<T>
where
    F: Future<Output = T>,
{
    let Ok(_held) = tokio::time::timeout(limit, lock.lock()).await else {
        let _ = WAITED.try_with(|waited| waited.set(true));
        return None;
    };
    Some(refresh().await)
}

/// Runs a door call, `call`, and says whether a refresh it asked for gave up waiting (`under`). A
/// refresh in a task the call starts is that task's own, and is not counted.
pub async fn noting_waits<F: Future>(call: F) -> (F::Output, bool) {
    WAITED
        .scope(Cell::new(false), async move {
            let output = call.await;
            (output, WAITED.with(Cell::get))
        })
        .await
}

/// `answer`, saying that a refresh it needed gave up waiting, so its numbers are the last ones.
pub fn mark_waiting(mut answer: serde_json::Value) -> serde_json::Value {
    if let Some(fields) = answer.as_object_mut() {
        fields.insert(WAITING.into(), WAITING_NOTE.into());
    }
    answer
}

/// A door call's answer (`call`'s), saying so exactly when a refresh it asked for gave up waiting.
pub async fn noted(call: impl Future<Output = Result<serde_json::Value, String>>) -> Result<serde_json::Value, String> {
    let (answer, waited) = noting_waits(call).await;
    if waited { answer.map(mark_waiting) } else { answer }
}

/// Whether a switch to `id` held, as the refresh after it shows (`switched_to_requested`): the
/// provider's account in use is that one. None when that refresh gave up waiting, so nothing shows.
pub fn switch_held(refreshed: Option<&Snapshot>, provider: Provider, id: &str) -> Option<bool> {
    refreshed.map(|snapshot| snapshot.accounts.iter().find(|a| a.provider == provider && a.active).map(|a| a.id.as_str()) == Some(id))
}

/// One refresh waiting for the lock, however many ask (`app::refresh_soon`): the first ask starts a
/// wait, those that come while it waits are that one, and once it has the lock the next ask starts
/// another.
pub struct Coalesce(AtomicBool);

impl Coalesce {
    pub const fn new() -> Self {
        Coalesce(AtomicBool::new(false))
    }

    /// Whether this ask starts a wait; false while one is waiting already.
    pub fn ask(&self) -> bool {
        !self.0.swap(true, Ordering::SeqCst)
    }

    /// The waiting one has the lock: the next ask starts a new wait.
    pub fn started(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// A lock that a panic elsewhere does not poison for us: what it guards is never half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Changes to the usage cache (`providers::Cache`) that wait for the next refresh, which makes them
/// under the refresh lock, so a refresh running meanwhile, which read the cache before, cannot write
/// it back over them: an account's backoff cleared (`retry`, after a sign-in or a disconnect), or
/// its entry dropped (`forget`, after a removal).
#[derive(Default)]
pub struct CacheDue(Mutex<(BTreeSet<String>, BTreeSet<String>)>);

impl CacheDue {
    pub fn retry(&self, key: String) {
        lock(&self.0).0.insert(key);
    }

    pub fn forget(&self, key: String) {
        lock(&self.0).1.insert(key);
    }

    /// Makes them in `cache` and empties the queue (a refresh, under the refresh lock).
    pub fn drain_into(&self, cache: &mut crate::providers::Cache) {
        let (retry, forget) = std::mem::take(&mut *lock(&self.0));
        for key in &retry {
            crate::providers::retry_now(cache, key);
        }
        for key in &forget {
            cache.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::Arc;

    /// How long a test waits for what should happen at once (a free lock taken). The wait ends as
    /// soon as it happens, so only a starved run waits at all, and none this long.
    const PATIENT: Duration = Duration::from_secs(5 * 60);
    /// A holder that never lets go ends by itself after this. The limit in the code ends a wait far
    /// sooner, however starved the run (tokio fires timers in deadline order); with the limit gone,
    /// the wait ends here instead and the test fails, rather than hanging.
    const SELF_ENDING: Duration = Duration::from_secs(60);
    /// The limit the tests give a wait for the lock: far inside SELF_ENDING.
    const SHORT: Duration = Duration::from_millis(100);
    /// A last backstop for each test.
    const TEST_LIMIT: Duration = Duration::from_secs(5 * 60);

    async fn within_test_limit<T>(test: impl Future<Output = T>) -> T {
        tokio::time::timeout(TEST_LIMIT, test).await.expect("the test ran past its limit: a limit in the code is missing")
    }

    /// Takes `lock` now and keeps it until SELF_ENDING, as an update's handover that never returns
    /// keeps it. Returns a flag set once it lets go.
    fn hold_for_good(lock: &Arc<tokio::sync::Mutex<()>>) -> Arc<AtomicBool> {
        let held = lock.clone().try_lock_owned().expect("a free lock");
        let let_go = Arc::new(AtomicBool::new(false));
        let flag = let_go.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SELF_ENDING).await;
            drop(held);
            flag.store(true, SeqCst);
        });
        let_go
    }

    #[tokio::test]
    async fn a_refresh_gives_up_on_a_lock_held_for_good_and_leaves_it_held() {
        within_test_limit(async {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            let let_go = hold_for_good(&lock);
            let ran = AtomicBool::new(false);
            let refreshed = under(&lock, SHORT, || async { ran.store(true, SeqCst) }).await;
            assert!(refreshed.is_none(), "a refresh gives up at its limit");
            assert!(!let_go.load(SeqCst), "the limit ended the wait, not the holder");
            assert!(!ran.load(SeqCst), "nothing runs without the lock");
            assert!(lock.try_lock().is_err(), "the holder keeps it");
        })
        .await
    }

    #[tokio::test]
    async fn a_free_lock_is_taken_and_held_through_the_refresh() {
        within_test_limit(async {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            assert_eq!(under(&lock, PATIENT, || async { lock.try_lock().is_err() }).await, Some(true), "the refresh runs under the lock");
            assert!(lock.try_lock().is_ok(), "and lets go of it");
            // One let go while it is waited for is taken then: a refresh that ends in time.
            let held = lock.clone().try_lock_owned().unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(SHORT).await;
                drop(held);
            });
            assert_eq!(under(&lock, PATIENT, || async { 7 }).await, Some(7));
        })
        .await
    }

    #[tokio::test]
    async fn a_door_call_hears_when_its_refresh_gave_up() {
        within_test_limit(async {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            let (answer, waited) = noting_waits(under(&lock, PATIENT, || async { 1 })).await;
            assert_eq!((answer, waited), (Some(1), false), "a refresh in time is nothing to tell");

            let _let_go = hold_for_good(&lock);
            assert_eq!(under(&lock, SHORT, || async {}).await, None, "outside a door call, nothing is told and nothing breaks");
            let (answer, waited) = noting_waits(under(&lock, SHORT, || async { 1 })).await;
            assert_eq!((answer, waited), (None, true));
            let (_, waited) = noting_waits(async {
                let lock = lock.clone();
                tokio::spawn(async move { under(&lock, SHORT, || async {}).await }).await.unwrap()
            })
            .await;
            assert!(!waited, "a refresh in a task the call started is that task's own");

            let snapshot = serde_json::json!({ "accounts": [], "updated_at": null });
            let marked = mark_waiting(snapshot.clone());
            assert_eq!(marked[WAITING], WAITING_NOTE);
            assert_eq!((marked["accounts"].clone(), marked.as_object().unwrap().len()), (snapshot["accounts"].clone(), 3), "the rest is as it was");
            assert_eq!(mark_waiting(serde_json::json!([1])), serde_json::json!([1]));
            assert!(WAITING_NOTE.chars().count() < 512, "agents get it whole (mcp's MAX_TEXT)");
        })
        .await
    }

    /// The door's answer carries the note exactly when a refresh it asked for gave up: never on an
    /// answer whose refresh ran, or that asked for none, and never in place of an error.
    #[tokio::test]
    async fn a_door_answer_is_noted_exactly_when_its_refresh_gave_up() {
        within_test_limit(async {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            let fresh = || async { serde_json::json!({ "accounts": [] }) };
            let unchanged = serde_json::json!({ "accounts": [] });
            assert_eq!(noted(async { Ok(serde_json::json!({ "op": "get_app_info" })) }).await, Ok(serde_json::json!({ "op": "get_app_info" })), "no refresh asked for");
            assert_eq!(noted(async { Ok(under(&lock, PATIENT, fresh).await.unwrap()) }).await, Ok(unchanged.clone()), "a refresh that ran");
            let _let_go = hold_for_good(&lock);
            let gave_up = noted(async { Ok(under(&lock, SHORT, fresh).await.unwrap_or_else(|| serde_json::json!({ "accounts": [] }))) }).await.unwrap();
            assert_eq!(gave_up, mark_waiting(unchanged.clone()), "a refresh that gave up");
            assert_eq!(noted(async { under(&lock, SHORT, fresh).await.map(Ok).unwrap_or(Err("busy".to_string())) }).await, Err("busy".into()), "an error stays an error");
        })
        .await
    }

    fn account(provider: Provider, id: &str, active: bool) -> crate::model::AccountUsage {
        serde_json::from_value(serde_json::json!({ "id": id, "provider": provider, "active": active, "sources": [], "windows": [],
            "resets": [], "stale": false, "monitor_connected": false })).unwrap()
    }

    #[test]
    fn a_switch_holds_when_its_account_is_in_use_and_is_unknown_when_its_refresh_gave_up() {
        let snapshot = |accounts| Snapshot { accounts, updated_at: None };
        let held = snapshot(vec![account(Provider::Codex, "x", true), account(Provider::Claude, "a", false), account(Provider::Claude, "b", true)]);
        assert_eq!(switch_held(Some(&held), Provider::Claude, "b"), Some(true));
        assert_eq!(switch_held(Some(&held), Provider::Claude, "a"), Some(false), "automatic switching moved away again");
        assert_eq!(switch_held(Some(&held), Provider::Claude, "x"), Some(false), "another provider's account");
        assert_eq!(switch_held(Some(&snapshot(vec![])), Provider::Claude, "b"), Some(false), "no account in use");
        assert_eq!(switch_held(None, Provider::Claude, "b"), None, "the refresh gave up: unknown, not false");
    }

    #[test]
    fn one_waits_however_many_ask() {
        let waiting = Coalesce::new();
        assert!(waiting.ask(), "the first ask starts a wait");
        assert!(!waiting.ask() && !waiting.ask(), "the next ones are that one");
        waiting.started();
        assert!(waiting.ask(), "once it has the lock, the next ask starts another");
        assert!(!waiting.ask());
    }

    fn cached(blocked: bool) -> crate::providers::Cached {
        serde_json::from_value(serde_json::json!({ "blocked_until": blocked.then_some("2999-01-01T00:00:00Z"), "credits": "7" })).unwrap()
    }

    fn blocked(cache: &crate::providers::Cache, key: &str) -> Option<bool> {
        cache.get(key).map(|c| !serde_json::to_value(c).unwrap()["blocked_until"].is_null())
    }

    #[test]
    fn the_cache_changes_left_for_a_refresh_are_made_once_for_their_own_accounts() {
        let mut cache: crate::providers::Cache = [("claude:a", true), ("claude:b", true), ("codex:c", true)].map(|(k, b)| (k.to_string(), cached(b))).into();
        let due = CacheDue::default();
        due.retry("claude:a".into());
        due.retry("claude:missing".into());
        due.forget("codex:c".into());
        due.drain_into(&mut cache);
        assert_eq!((blocked(&cache, "claude:a"), blocked(&cache, "claude:b"), blocked(&cache, "codex:c")), (Some(false), Some(true), None),
            "a's backoff cleared, b's kept, c dropped");
        assert!(!cache.contains_key("claude:missing"), "nothing made up");
        assert_eq!(serde_json::to_value(&cache["claude:a"]).unwrap()["credits"], "7", "the last numbers kept");
        cache.insert("claude:a".into(), cached(true));
        cache.insert("codex:c".into(), cached(true));
        due.drain_into(&mut cache);
        assert_eq!((blocked(&cache, "claude:a"), blocked(&cache, "codex:c")), (Some(true), Some(true)), "made once: the queue is empty after");
    }

    #[test]
    fn a_door_call_that_waited_still_answers_before_its_client_gives_up() {
        let app = normalized(include_str!("app.rs"));
        let client = function(&app, "fn http_client(");
        assert!(has(client, ".timeout(Duration::from_secs(20))"), "{client}");
        assert!(LOCK_WAIT + Duration::from_secs(20) < crate::door::CLIENT_TIMEOUT, "the wait and one request of its own fit in the door's time");
        assert!(LOCK_WAIT >= Duration::from_secs(30), "a refresh normally ends well inside it");
    }

    // ------------------------------------------------------------------------------------------
    // Reading the source. app.rs needs a running app; what it does with the helpers above is one
    // call each, and these checks read those calls, as code: comments dropped, literals emptied and
    // line breaks ignored (`normalized`).

    fn named(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// The length of the raw string literal (`r".."`, `r#".."#`, `br".."`) at `i`, if one starts there.
    fn raw_string(chars: &[char], i: usize) -> Option<usize> {
        let mut j = i + usize::from(chars.get(i) == Some(&'b'));
        if chars.get(j) != Some(&'r') {
            return None;
        }
        j += 1;
        let hashes = chars[j..].iter().take_while(|&&c| c == '#').count();
        j += hashes;
        if chars.get(j) != Some(&'"') {
            return None;
        }
        j += 1;
        while j < chars.len() {
            if chars[j] == '"' && chars[j + 1..].iter().take(hashes).filter(|&&c| c == '#').count() == hashes {
                return Some(j + 1 + hashes - i);
            }
            j += 1;
        }
        Some(chars.len() - i)
    }

    /// `source` as code: comments dropped, string and char literals emptied (`""`, `''`), and
    /// whitespace kept only between two parts of a name ("let x", but "a.b" for "a . b"), so a check
    /// reads code however rustfmt lays it out, and never text in a literal or a comment.
    fn normalized(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut gap = false;
        let mut i = 0;
        let emit = |out: &mut String, gap: &mut bool, text: &str| {
            if *gap && out.chars().next_back().is_some_and(named) && text.chars().next().is_some_and(named) {
                out.push(' ');
            }
            *gap = false;
            out.push_str(text);
        };
        while i < chars.len() {
            let (c, next) = (chars[i], chars.get(i + 1).copied());
            let after_name = out.chars().next_back().is_some_and(named) && !gap;
            if c.is_whitespace() {
                gap = true;
                i += 1;
            } else if c == '/' && next == Some('/') {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                gap = true;
            } else if c == '/' && next == Some('*') {
                let mut depth = 0;
                while i < chars.len() {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                gap = true;
            } else if let Some(len) = (!after_name).then(|| raw_string(&chars, i)).flatten() {
                emit(&mut out, &mut gap, "\"\"");
                i += len;
            } else if c == '"' {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += if chars[i] == '\\' { 2 } else { 1 };
                }
                i += 1;
                emit(&mut out, &mut gap, "\"\"");
            } else if c == '\'' && (next == Some('\\') || chars.get(i + 2) == Some(&'\'')) {
                i += if next == Some('\\') { 3 } else { 2 };
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
                i += 1;
                emit(&mut out, &mut gap, "''");
            } else {
                emit(&mut out, &mut gap, c.encode_utf8(&mut [0; 4]));
                i += 1;
            }
        }
        out
    }

    fn has(code: &str, needle: &str) -> bool {
        code.contains(&normalized(needle))
    }

    /// The item in `code` (normalized) that starts with `signature`, through its closing brace.
    fn function<'a>(code: &'a str, signature: &str) -> &'a str {
        let signature = normalized(signature);
        let start = code.find(&signature).unwrap_or_else(|| panic!("no `{signature}`"));
        let open = start + code[start..].find('{').expect("a body");
        let mut depth = 0;
        for (i, c) in code[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &code[start..=open + i];
                    }
                }
                _ => {}
            }
        }
        panic!("`{signature}` never closes")
    }

    /// Every source file under src, by its path from there, normalized.
    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                    out.push((name, normalized(&std::fs::read_to_string(&path).unwrap())));
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = vec![];
        walk(&root, &root, &mut out);
        out.sort();
        assert!(out.iter().any(|(name, _)| name == "app.rs") && out.len() > 20, "the sources: {:?}", out.iter().map(|(n, _)| n).collect::<Vec<_>>());
        out
    }

    /// Up to `n` characters of `code` on each side of byte `at`, for a message.
    fn around(code: &str, at: usize, n: usize) -> &str {
        let mut start = at.saturating_sub(n);
        while !code.is_char_boundary(start) {
            start -= 1;
        }
        let mut end = (at + n).min(code.len());
        while !code.is_char_boundary(end) {
            end += 1;
        }
        &code[start..end]
    }

    #[test]
    fn the_source_is_read_as_code() {
        let code = normalized("let a = \"refresh_lock.lock()\"; // refresh_lock\n/* refresh_lock */ let b = r#\"x\"#;\nlet c: &'static str = x . y ( '{' ) ;");
        assert_eq!(code, "let a=\"\";let b=\"\";let c:&'static str=x.y('');");
        assert!(!code.contains("refresh_lock"), "not in a literal or a comment");
        assert_eq!(function("fn f(){if x{y}}fn g(){}", "fn f("), "fn f(){if x{y}}");
    }

    /// Every wait for the refresh lock, in every source file, is one of three: a refresh's, within
    /// LOCK_WAIT (`under`); `refresh_soon`'s, which holds nothing and which nobody waits for; and,
    /// in builds that wait for a refresh before an update's installer, the install's, within its
    /// step's limit (`update_check::REFRESH_WAIT_LIMIT`). Anything else that names the lock is the
    /// field and its value, or the handle that install takes. A new kind fails here, naming it.
    #[test]
    fn every_wait_for_the_refresh_lock_is_bounded_or_holds_nothing() {
        let (mut bounded, mut soon, mut install, mut unknown) = (0, 0, 0, vec![]);
        for (file, code) in sources() {
            for (at, word) in code.match_indices("refresh_lock") {
                let (before, after) = (&code[..at], &code[at + word.len()..]);
                if before.chars().next_back().is_some_and(named) || after.chars().next().is_some_and(named) {
                    continue;
                }
                match file.as_str() {
                    "app.rs" if after.starts_with(':') && !after.starts_with("::") && !before.ends_with("let ") => {}
                    "app.rs" if before.ends_with("refresh_wait::under(&state.") && after.starts_with(",refresh_wait::LOCK_WAIT,") => bounded += 1,
                    "app.rs" if before.ends_with("drop(app.state::<AppState>().") && after.starts_with(".lock().await)") => soon += 1,
                    "updates.rs" if before.ends_with("let ") && after.starts_with("=app.state::<AppState>().refresh_lock.clone();") => {}
                    "updates.rs" if before.ends_with("let refresh_lock=app.state::<AppState>().") && after.starts_with(".clone();") => {}
                    "updates.rs" if before.ends_with("update_check::REFRESH_WAIT_LIMIT,async move{Ok::<_,String>(") && after.starts_with(".lock_owned().await)}") => install += 1,
                    _ => unknown.push(format!("{file}: …{}…", around(&code, at, 70))),
                }
            }
        }
        assert!(unknown.is_empty(), "a wait for the refresh lock with no limit, or that holds something (or a new kind of use):\n{}", unknown.join("\n"));
        assert_eq!((bounded, soon), (1, 1), "the refresh's bounded wait and refresh_soon's");
        assert!(install <= 1, "the install's, in builds that wait for a refresh");

        let app = normalized(include_str!("app.rs"));
        let within = function(&app, "async fn refresh_within(");
        assert!(has(within, "let refreshed = refresh_wait::under(&state.refresh_lock, refresh_wait::LOCK_WAIT, || refresh_locked(app)).await;"), "{within}");
        assert!(has(within, "if refreshed.is_none() { refresh_soon(app); } refreshed"), "a refresh that gave up is followed by one once the lock is free:\n{within}");
        let soon = function(&app, "fn refresh_soon(");
        let spawned = soon.find(&normalized("tauri::async_runtime::spawn(async move {")).expect("nobody waits for it");
        assert!(has(&soon[..spawned], "if !WAITING.ask() { return; }"), "one task however many ask:\n{soon}");
        assert!(has(&soon[spawned..], "drop(app.state::<AppState>().refresh_lock.lock().await); WAITING.started(); do_refresh(&app).await;"), "{soon}");
        assert!(has(&app, "static WAITING: refresh_wait::Coalesce = refresh_wait::Coalesce::new();"));
        let fallback = function(&app, "async fn do_refresh(");
        assert!(has(fallback, "match refresh_within(app).await { Some(snapshot) => snapshot, None => app.state::<AppState>().snapshot.lock().unwrap().clone(), }"),
            "a refresh that gave up returns the last numbers:\n{fallback}");
        let locked = function(&app, "async fn refresh_locked(");
        let read = locked.find(&normalized("let settings = state.settings.lock().unwrap().clone();")).expect("the refresh reads the settings");
        assert!(has(&locked[..read], "state.cache_due.drain_into(&mut state.usage_cache.lock().unwrap());"), "the cache changes left to it, first:\n{locked}");
        assert!(has(&locked[..read], "sharing.catch_up(&mut sharing::Files { interval })"), "what saves left, before it reads the settings:\n{locked}");
    }

    /// `settings_lock` is never held while anything waits: `store_settings` cannot wait (it is no
    /// async fn), takes the lock's guard and lets go of it, and never refreshes; nothing refreshes
    /// between taking the lock and handing it to `store_settings`.
    #[test]
    fn nothing_waits_while_holding_settings_lock() {
        let app = normalized(include_str!("app.rs"));
        let refreshes = ["do_refresh(", "refresh_within(", "refresh_soon(", "refresh_locked("];
        let store = function(&app, "fn store_settings(");
        assert!(has(&app, "\nfn store_settings(app: &AppHandle, held: tokio::sync::MutexGuard<'_, ()>, settings: Settings) -> Result<bool, String> {")
            && !has(&app, "async fn store_settings("), "{store}");
        assert!(has(store, "let refresh = state.sharing.saved(&previous, &settings); drop(held); Ok(refresh)"), "what the save moved is left to the refresh it asks for:\n{store}");
        for call in refreshes {
            assert!(!store.contains(call), "store_settings refreshes (`{call}`) while its caller holds settings_lock:\n{store}");
        }
        let mut taken = 0;
        let mut from = 0;
        while let Some(at) = app[from..].find("settings_lock.lock().await").map(|i| i + from) {
            taken += 1;
            from = at + 1;
            let store = at + app[at..].find("store_settings(").expect("each settings change ends in store_settings");
            let held = &app[at..store];
            for call in refreshes {
                assert!(!held.contains(call), "a refresh (`{call}`) while settings_lock is held:\n{held}");
            }
            assert!(app[store..].starts_with("store_settings(&app,held,") || app[store..].starts_with("store_settings(app,held,"), "the lock goes with the change:\n{}", around(&app, store, 80));
        }
        assert_eq!(taken, 6, "save_settings, update_settings, apply_backup, remove_one, set_project_pin, the compact toggle");
        assert!(has(&app, "let held = state.settings_lock.lock().await; if store_settings(&app, held, settings)? { do_refresh(&app).await; } Ok(())"), "save_settings refreshes when the save asks");
        assert!(has(&app, "if store_settings(&app, held, next.clone())? { do_refresh(&app).await; } value(next)"), "update_settings refreshes when the save asks");
        let restore = function(&app, "async fn apply_backup(");
        let stored = restore.find(&normalized("store_settings(app, held, merged_settings)?;")).expect("a restore stores its settings");
        assert_eq!((restore.matches("do_refresh(").count(), restore[stored..].matches("do_refresh(app).await;").count()), (1, 1), "a restore refreshes once, at its end:\n{restore}");
    }

    /// What app.rs does with the rest: an account's reservation ends before anything waits for a
    /// refresh, the cache and sharing changes go through their queues, and a door call says when
    /// its refresh waited.
    #[test]
    fn the_app_hands_its_waits_to_the_helpers() {
        let app = normalized(include_str!("app.rs"));
        let disconnect = function(&app, "async fn disconnect_one(");
        let dropped = disconnect.find(&normalized("drop(operation);")).expect("the reservation ends");
        assert!(dropped < disconnect.find(&normalized("retry_soon(app, monitoring::key(&slot));")).expect("its backoff is left to the next refresh"), "{disconnect}");
        assert!(has(function(&app, "fn retry_soon("), "app.state::<AppState>().cache_due.retry(key);"), "a cleared backoff is queued for the next refresh");
        assert!(has(function(&app, "async fn remove_one("), "app.state::<AppState>().cache_due.forget(monitoring::key(&slot));"), "so is a removed account's cache entry");
        assert!(!function(&app, "async fn remove_one(").contains("usage_cache"), "and nothing else touches the cache there");
        let add = function(&app, "async fn start_add_account(");
        let dropped = add.find(&normalized("drop(pending);")).expect("the reservation ends");
        assert!(dropped > add.find("job.wait(").unwrap() && dropped < add.find(&normalized("do_refresh(&app).await")).unwrap(), "it covers the sign-in, not the refresh:\n{add}");
        let login = function(&app, "async fn start_monitoring_login(");
        assert!(has(login, "retry_soon(&app, key); refresh_soon(&app);"), "{login}");

        assert!(has(&app, "door::serve(door::pipe_name(), move |request| door_reply(h.clone(), request))"), "the door answers through door_reply");
        assert!(has(function(&app, "async fn door_reply("), "refresh_wait::noted(door_call(app, request)).await"), "noted exactly when a refresh waited");
        let switch = function(&app, "async fn agent_switch(");
        assert!(has(switch, "let refreshed = refresh_within(app).await; let held = refresh_wait::switch_held(refreshed.as_ref(), provider, &id);")
            && has(switch, "result[\"switched_to_requested\"] = held.into();"), "unknown (null) when its refresh waited:\n{switch}");

        let sync = function(&app, "async fn sync_roster(");
        assert!(has(sync, "sharing.sync_roster(&place, live, |sync| {"), "a roster sync only where the settings still share:\n{sync}");
        let locked = function(&app, "async fn refresh_locked(");
        assert!(has(locked, "if !sharing::still(&place, &sharing::Place::of(&h.state::<AppState>().settings.lock().unwrap())) { return None; }"),
            "a refresh's presence record only where the settings still share:\n{locked}");
        assert_eq!(app.matches("presence::withdraw(").count(), 1, "presence is withdrawn on exit, and otherwise by the refresh (sharing::Files), never by a save");
        assert!(has(function(&app, "pub(crate) fn on_exit("), "presence::withdraw("));
        assert!(!app.contains("roster::withdraw("), "the roster is withdrawn by the refresh (sharing::Files), never by a save");
    }
}
