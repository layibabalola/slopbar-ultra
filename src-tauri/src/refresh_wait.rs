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
pub const WAITING_NOTE: &str = "SlopBar is still busy with an earlier refresh, so these are the last numbers it has. \
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
    use crate::source_guard::{args, calls, closure_body, count, find, function, has, lost, places, statement, tokens, update};
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

    /// A refresh gives up at the limit it is given, not at LOCK_WAIT: a lock let go halfway to
    /// LOCK_WAIT is still given up on, however starved the run (the earlier timer fires first).
    #[tokio::test]
    async fn a_refresh_gives_up_at_the_limit_it_is_given() {
        within_test_limit(async {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            let held = lock.clone().try_lock_owned().expect("a free lock");
            tokio::spawn(async move {
                tokio::time::sleep(LOCK_WAIT / 2).await;
                drop(held);
            });
            assert_eq!(under(&lock, SHORT, || async { 1 }).await, None, "given up after SHORT, though the lock is let go before LOCK_WAIT");
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
        let app = tokens(include_str!("app.rs"));
        let client = function(&app, "app.rs", "fn http_client(", CLIENT);
        let timeout = ".timeout(Duration::from_secs(20))";
        assert!(has(client, timeout), "{}\n{client}", lost("http_client in app.rs", timeout, CLIENT));
        assert!(LOCK_WAIT + Duration::from_secs(20) < crate::door::CLIENT_TIMEOUT, "the wait and one request of its own fit in the door's time");
        assert!(LOCK_WAIT >= Duration::from_secs(30), "a refresh normally ends well inside it");
    }

    // ------------------------------------------------------------------------------------------
    // Reading the source. app.rs needs a running app; what it does with the helpers above is one
    // call each, and these checks read those calls as the source guards read code (`source_guard`):
    // tokens, not layout, and a call that takes a closure argument by argument, so a default
    // rustfmt run over app.rs reads the same. Each call is read with the statements around it, so
    // one that is skipped (`if false`, a block that never runs) no longer reads the same.

    /// Where each guard's needles are, for its failure (see `source_guard::update`).
    const CLIENT: &str = "refresh_wait::tests::a_door_call_that_waited_still_answers_before_its_client_gives_up (src-tauri/src/refresh_wait.rs)";
    const BOUNDED: &str = "refresh_wait::tests::every_wait_for_the_refresh_lock_is_bounded_or_holds_nothing (src-tauri/src/refresh_wait.rs)";
    const SETTINGS: &str = "refresh_wait::tests::nothing_waits_while_holding_settings_lock (src-tauri/src/refresh_wait.rs)";
    const HANDS: &str = "refresh_wait::tests::the_app_hands_its_waits_to_the_helpers (src-tauri/src/refresh_wait.rs)";
    const ROSTER: &str = "refresh_wait::tests::the_roster_is_waited_for_on_the_blocking_pool_only (src-tauri/src/refresh_wait.rs)";
    const WIRES: &str = "refresh_wait::tests::the_app_wires_the_exit_and_the_due_file (src-tauri/src/refresh_wait.rs)";
    const REAL: &str = "refresh_wait::tests::the_real_withdrawals_go_to_the_place_they_are_given (src-tauri/src/refresh_wait.rs)";

    fn named(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// Every source file under src, by its path from there, as the guards read it.
    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                    out.push((name, tokens(&std::fs::read_to_string(&path).unwrap())));
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

    /// The code of `scope` before and after `call`, a call's brackets in it as `calls` gives them.
    fn either_side<'a>(scope: &'a str, call: &str) -> (&'a str, &'a str) {
        let at = call.as_ptr() as usize - scope.as_ptr() as usize;
        (&scope[..at], &scope[at + call.len()..])
    }

    /// The one call of `name` in `scope`, which `test` reads in `place`.
    fn one_call<'a>(scope: &'a str, name: &str, place: &str, test: &str) -> &'a str {
        let found = calls(scope, name, test);
        assert_eq!(found.len(), 1, "{place} calls `{name}` once. {}\n{scope}", update(test));
        found[0]
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
            for at in places(&code, "refresh_lock") {
                let (before, after) = (&code[..at], &code[at + "refresh_lock".len()..]);
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
        assert_eq!((bounded, soon), (1, 1), "the refresh's bounded wait and refresh_soon's. {}", update(BOUNDED));
        assert!(install <= 1, "the install's, in builds that wait for a refresh. {}", update(BOUNDED));

        let app = tokens(include_str!("app.rs"));
        let within = function(&app, "app.rs", "async fn refresh_within(", BOUNDED);
        let under = one_call(within, "refresh_wait::under", "refresh_within in app.rs", BOUNDED);
        let [lock, limit, refresh] = args(under)[..] else { panic!("refresh_wait::under takes the lock, a limit and a refresh: {under}. {}", update(BOUNDED)) };
        assert_eq!((lock, limit, closure_body(refresh)), ("&state.refresh_lock", "refresh_wait::LOCK_WAIT", Some("refresh_locked(app)")),
            "the refresh runs under the refresh lock, within LOCK_WAIT. {}\n{within}", update(BOUNDED));
        let (before, after) = either_side(within, under);
        assert!(before == tokens("async fn refresh_within(app: &AppHandle) -> Option<Snapshot> { let state = app.state::<AppState>(); let refreshed = refresh_wait::under")
            && after == tokens(".await; if refreshed.is_none() { refresh_soon(app); } refreshed }"),
            "a refresh that gave up is followed by one once the lock is free. {}\n{within}", update(BOUNDED));
        let soon = function(&app, "app.rs", "fn refresh_soon(", BOUNDED);
        assert!(has(soon, "static WAITING: refresh_wait::Coalesce = refresh_wait::Coalesce::new(); if !WAITING.ask() { return; } \
            let app = app.clone(); tauri::async_runtime::spawn(async move { \
            drop(app.state::<AppState>().refresh_lock.lock().await); WAITING.started(); do_refresh(&app).await; }); }"),
            "one task however many ask, which nobody waits for. {}\n{soon}", update(BOUNDED));
        let fallback = function(&app, "app.rs", "async fn do_refresh(", BOUNDED);
        assert!(has(fallback, "match refresh_within(app).await { Some(snapshot) => snapshot, None => app.state::<AppState>().snapshot.lock().unwrap().clone(), }"),
            "a refresh that gave up returns the last numbers. {}\n{fallback}", update(BOUNDED));
        let locked = function(&app, "app.rs", "async fn refresh_locked(", BOUNDED);
        let reads = "let settings = state.settings.lock().unwrap().clone();";
        let read = find(locked, reads).unwrap_or_else(|| panic!("{}", lost("refresh_locked in app.rs", reads, BOUNDED)));
        let first = &locked[..read];
        let catch_up = one_call(first, "tauri::async_runtime::spawn_blocking", "refresh_locked in app.rs, before it reads the settings", BOUNDED);
        let [withdraw] = args(catch_up)[..] else { panic!("spawn_blocking takes one closure: {catch_up}. {}", update(BOUNDED)) };
        assert!(withdraw.starts_with("move||") && closure_body(withdraw) == Some(tokens("sharing.catch_up(&sharing::Files { interval }, sharing::WITHDRAW_LIMIT)").as_str()),
            "what saves left is withdrawn on the blocking pool, within its limit. {}\n{first}", update(BOUNDED));
        let (before, after) = either_side(first, catch_up);
        assert!(before.ends_with(&tokens("let state = app.state::<AppState>(); state.cache_due.drain_into(&mut state.usage_cache.lock().unwrap()); \
            if state.sharing.is_due() { let interval = state.settings.lock().unwrap().refresh_minutes.max(1) as u64 * 60; \
            let sharing = state.sharing.clone(); let reset = tauri::async_runtime::spawn_blocking"))
            && after.starts_with(&tokens(".await.unwrap_or(false); if reset { *state.roster_seen.lock().unwrap() = roster::Seen::default(); } }")),
            "first, under the lock: the cache changes left to it, then what saves left, before it reads the settings. {}\n{first}", update(BOUNDED));
    }

    /// `settings_lock` is never held while anything waits: `store_settings` cannot wait (it is no
    /// async fn), takes the lock's guard and lets go of it, and never refreshes; nothing waits (for
    /// I/O, a refresh or anything else) between taking the lock and handing it to `store_settings`.
    #[test]
    fn nothing_waits_while_holding_settings_lock() {
        let app = tokens(include_str!("app.rs"));
        let refreshes = ["do_refresh(", "refresh_within(", "refresh_soon(", "refresh_locked("];
        let store = function(&app, "app.rs", "fn store_settings(", SETTINGS);
        assert!(has(&app, "fn store_settings(app: &AppHandle, held: tokio::sync::MutexGuard<'_, ()>, settings: Settings) -> Result<bool, String> {")
            && !has(&app, "async fn store_settings("), "store_settings is no async fn and takes the lock's guard. {}\n{store}", update(SETTINGS));
        assert!(has(store, "let refresh = state.sharing.saved(&previous, &settings); drop(held); Ok(refresh)"),
            "what the save moved is left to the refresh it asks for. {}\n{store}", update(SETTINGS));
        for call in refreshes {
            assert!(!has(store, call), "store_settings refreshes (`{call}`) while its caller holds settings_lock:\n{store}");
        }
        let taking = "settings_lock.lock().await";
        let mut taken = 0;
        for at in places(&app, taking) {
            taken += 1;
            let store = at + find(&app[at..], "store_settings(").expect("each settings change ends in store_settings");
            let held = &app[at..store];
            for call in refreshes {
                assert!(!has(held, call), "a refresh (`{call}`) while settings_lock is held:\n{held}");
            }
            assert!(!has(&held[taking.len()..], ".await"), "a wait while settings_lock is held:\n{held}");
            assert!(app[store..].starts_with("store_settings(&app,held,") || app[store..].starts_with("store_settings(app,held,"), "the lock goes with the change:\n{}", around(&app, store, 80));
        }
        assert_eq!(taken, 7, "save_settings, update_settings, apply_backup, remove_one, set_project_pin, the compact toggle, the sharing setup. {}", update(SETTINGS));
        assert!(has(&app, "let held = state.settings_lock.lock().await; if store_settings(&app, held, settings)? { do_refresh(&app).await; } Ok(())"),
            "save_settings refreshes when the save asks. {}", update(SETTINGS));
        assert!(has(&app, "if store_settings(&app, held, next.clone())? { do_refresh(&app).await; } value(next)"), "update_settings refreshes when the save asks. {}", update(SETTINGS));
        let restore = function(&app, "app.rs", "async fn apply_backup(", SETTINGS);
        let stores = "store_settings(app, held, merged_settings)?;";
        let stored = find(restore, stores).unwrap_or_else(|| panic!("{}", lost("apply_backup in app.rs", stores, SETTINGS)));
        assert_eq!((count(restore, "do_refresh("), count(&restore[stored..], "do_refresh(app).await;")), (1, 1), "a restore refreshes once, at its end:\n{restore}");
    }

    /// What app.rs does with the rest: an account's reservation ends before anything waits for a
    /// refresh, the cache and sharing changes go through their queues, and a door call says when
    /// its refresh waited.
    #[test]
    fn the_app_hands_its_waits_to_the_helpers() {
        let app = tokens(include_str!("app.rs"));
        let disconnect = function(&app, "app.rs", "async fn disconnect_one(", HANDS);
        assert!(has(disconnect, "drop(operation); retry_soon(app, monitoring::key(&slot)); Ok(revoked) }"),
            "the reservation ends, then the backoff is left to the next refresh. {}\n{disconnect}", update(HANDS));
        assert!(has(&app, "fn retry_soon(app: &AppHandle, key: String) { app.state::<AppState>().cache_due.retry(key); }"), "a cleared backoff is queued for the next refresh. {}", update(HANDS));
        let remove = function(&app, "app.rs", "async fn remove_one(", HANDS);
        assert!(has(remove, "app.state::<AppState>().monitor.disconnect(&slot, &http_client()).await?; app.state::<AppState>().cache_due.forget(monitoring::key(&slot)); }"),
            "so is a removed account's cache entry. {}\n{remove}", update(HANDS));
        assert!(!has(remove, "usage_cache"), "and nothing else touches the cache there:\n{remove}");
        let add = function(&app, "app.rs", "async fn start_add_account(", HANDS);
        assert!(has(add, "let added = job.wait(&http_client()).await; drop(session); drop(pending); if let Some(slot) = added {")
            && find(add, "job.wait(") < find(add, "do_refresh(&app).await"), "it covers the sign-in, not the refresh. {}\n{add}", update(HANDS));
        let login = function(&app, "app.rs", "async fn start_monitoring_login(", HANDS);
        assert!(has(login, "retry_soon(&app, key); refresh_soon(&app); outcome"), "{}\n{login}", update(HANDS));

        let serve = one_call(&app, "door::serve", "app.rs", HANDS);
        let [pipe, handle] = args(serve)[..] else { panic!("door::serve takes the pipe and what answers it: {serve}. {}", update(HANDS)) };
        assert!(pipe == "door::pipe_name()" && handle.starts_with("move|request|") && closure_body(handle) == Some("door_reply(h.clone(),request)"),
            "the door answers through door_reply. {}\n{serve}", update(HANDS));
        assert!(has(&app, "async fn door_reply(app: AppHandle, request: door::Request) -> Result<serde_json::Value, String> { refresh_wait::noted(door_call(app, request)).await }"),
            "noted exactly when a refresh waited. {}", update(HANDS));
        let switch = function(&app, "app.rs", "async fn agent_switch(", HANDS);
        let fallback = one_call(switch, "refreshed.unwrap_or_else", "agent_switch in app.rs", HANDS);
        let [last] = args(fallback)[..] else { panic!("unwrap_or_else takes one closure: {fallback}. {}", update(HANDS)) };
        assert_eq!(closure_body(last), Some(tokens("app.state::<AppState>().snapshot.lock().unwrap().clone()").as_str()), "the last numbers, when its refresh waited. {}\n{switch}", update(HANDS));
        let (before, after) = either_side(switch, fallback);
        assert!(before.ends_with(&tokens("let refreshed = refresh_within(app).await; let held = refresh_wait::switch_held(refreshed.as_ref(), provider, &id); let snapshot = refreshed.unwrap_or_else"))
            && after.starts_with(&tokens("; let mut result = serde_json::to_value(snapshot).map_err(|e| e.to_string())?; result[\"switched_to_requested\"] = held.into(); result[\"desktop\"] =")),
            "and unknown (null), when its refresh waited. {}\n{switch}", update(HANDS));

        let sync = function(&app, "app.rs", "async fn sync_roster(", HANDS);
        let lives = "let live =";
        let live = statement(sync, lives).unwrap_or_else(|| panic!("{}", lost("sync_roster in app.rs", lives, HANDS)));
        assert_eq!(live.strip_prefix(&tokens(lives)).and_then(|v| v.strip_suffix(';')).and_then(closure_body), Some(tokens("sharing::Place::of(&h.state::<AppState>().settings.lock().unwrap())").as_str()),
            "the live settings, read where the roster's lock is held. {}\n{sync}", update(HANDS));
        let synced = one_call(sync, "sharing.sync_roster", "sync_roster in app.rs", HANDS);
        let [place, read, run] = args(synced)[..] else { panic!("sharing.sync_roster takes the place, the live settings and the sync: {synced}. {}", update(HANDS)) };
        assert!(place == "&place" && read == "live" && closure_body(run) == Some(tokens("{ if let Some(run) = run { sync.offer_run(run); } \
            sync.withhold_readings = !share; sync.sync(store.as_ref(), &passphrase, (id, name), &accounts, now, force, soon); sync.seen.clone() }").as_str()) && run.starts_with("|sync|"),
            "a roster sync only where the settings still share. {}\n{sync}", update(HANDS));
        // A sync that could not run leaves no proxy reason from an earlier one (`Seen::unsynced`).
        assert!(has(sync, "else { state.roster_seen.lock().unwrap().unsynced(); return; };")
            && has(sync, "match seen { Some(seen) => *state.roster_seen.lock().unwrap() = seen, None => state.roster_seen.lock().unwrap().unsynced(), }"),
            "{}\n{sync}", update(HANDS));
        let locked = function(&app, "app.rs", "async fn refresh_locked(", HANDS);
        let identity = "let Some((id, name)) = presence::identity() else {";
        let unknown = statement(locked, identity).unwrap_or_else(|| panic!("{}", lost("refresh_locked in app.rs", identity, HANDS)));
        // rustfmt ends the `return` with a semicolon.
        assert!([tokens("let Some((id, name)) = presence::identity() else { return Some((vec![], health::SharingResult::failed(now))) };"),
            tokens("let Some((id, name)) = presence::identity() else { return Some((vec![], health::SharingResult::failed(now))); };")].iter().any(|s| s == unknown),
            "{}\n{locked}", lost("refresh_locked in app.rs", identity, HANDS));
        let next = &locked[unknown.as_ptr() as usize - locked.as_ptr() as usize + unknown.len()..];
        assert!(next.starts_with(&tokens("if !sharing::still(&place, &sharing::Place::of(&h.state::<AppState>().settings.lock().unwrap())) { return None; } let wrote = store.write(")),
            "a refresh's presence record only where the settings still share. {}\n{locked}", update(HANDS));
        // What Health shows of it is worked out by the helper health's tests cover.
        assert!(has(next, "let wrote = store.write(&presence::own(id, name, interval, &mine, now)); let listed = store.list(); \
            let result = health::SharingResult::of(now, &wrote, &listed);"), "{}\n{locked}", update(HANDS));
        assert!(!has(&app, "presence::withdraw(") && !has(&app, "roster::withdraw("),
            "this machine's files are withdrawn through sharing (sharing::Files): by the refresh and on exit, never by a save. {}", update(HANDS));
        let exit = function(&app, "app.rs", "pub(crate) fn on_exit(", HANDS);
        assert!(has(exit, ".sharing.exit("), "the exit withdraws where sharing is and what saves left. {}\n{exit}", update(HANDS));
    }

    /// `Sharing::with_syncer`, `sync_roster` and `catch_up` wait for the roster's lock, which a sync
    /// that hangs on a folder holds: app.rs calls them on the blocking pool only (`spawn_blocking`),
    /// never on a thread of the runtime, which would then stay pinned. The exit's own part for
    /// sharing runs on a thread of its own, within its limit (update_check's guard of `on_exit`).
    #[test]
    fn the_roster_is_waited_for_on_the_blocking_pool_only() {
        let app = tokens(include_str!("app.rs"));
        let blocking = calls(&app, "spawn_blocking", ROSTER);
        for (method, at_least) in [(".with_syncer(", 2), (".sync_roster(", 1), (".catch_up(", 1)] {
            let inside: usize = blocking.iter().map(|call| count(call, method)).sum();
            assert!(inside == count(&app, method) && inside >= at_least, "`{method}` runs within spawn_blocking only ({inside} of {}). {}", count(&app, method), update(ROSTER));
        }
    }

    /// How app.rs wires `Sharing`, which sharing's tests drive through a fake: Tauri's exit event runs
    /// `on_exit`; its part for sharing hands the exit where the live settings share, with the
    /// interval a refresh's presence record states, within WITHDRAW_LIMIT; and the app's `Sharing`
    /// keeps what is due in the due file, so the next start finds it.
    #[test]
    fn the_app_wires_the_exit_and_the_due_file() {
        let app = tokens(include_str!("app.rs"));
        let event = "if matches!(event, tauri::RunEvent::Exit) { on_exit(app); }";
        assert!(has(&app, event), "{}", lost("app.rs", event, WIRES));
        let exit = function(&app, "app.rs", "pub(crate) fn on_exit(", WIRES);
        let shared = one_call(exit, ".sharing.exit", "on_exit in app.rs", WIRES);
        assert_eq!(args(shared), ["&sharing::Place::of(&settings)", "&files", "sharing::WITHDRAW_LIMIT"], "the exit, where sharing is now. {}\n{exit}", update(WIRES));
        for (start, want) in [("let settings =", "let settings = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();"),
            ("let files =", "let files = sharing::Files { interval: settings.refresh_minutes.max(1) as u64 * 60 };")] {
            assert_eq!(statement(exit, start), Some(tokens(want).as_str()), "{}\n{exit}", lost("on_exit in app.rs", want, WIRES));
            // Bound once: a second binding of the same name would hand the exit something else.
            let binding = start.trim_end_matches(" =");
            assert_eq!(count(exit, binding), 1, "`{binding}` once in on_exit in app.rs. {}\n{exit}", update(WIRES));
        }
        let kept = one_call(&app, "sharing::Sharing::kept_in", "app.rs", WIRES);
        assert_eq!(args(kept), [tokens("settings::app_dir().expect(\"config directory\").join(sharing::DUE_FILE)")], "the due file. {}", update(WIRES));
        let field = "sharing: std::sync::Arc::new(sharing::Sharing::kept_in(";
        assert!(has(&app, field), "the app's own Sharing keeps it. {}", lost("app.rs", field, WIRES));
    }

    /// `sharing`'s tests withdraw through a fake: the real withdrawals (`sharing::Files`) need this
    /// machine's identity from the app's data folder, so this check reads them. Each opens the
    /// stores of the place it is given (`Place::settings`, tested there) and withdraws there; and
    /// forgetting the real roster sync is its whole reset.
    #[test]
    fn the_real_withdrawals_go_to_the_place_they_are_given() {
        let sharing = tokens(include_str!("sharing.rs"));
        let files = function(&sharing, "sharing.rs", "impl Withdraw for Files {", REAL);
        assert!(has(files, "fn presence(&self, place: &Place) { if let Some(store) = crate::presence::store(&place.settings()) { \
            crate::presence::withdraw(store.as_ref(), self.interval); } }"), "{}\n{files}", update(REAL));
        assert!(has(files, "fn roster(&self, place: &Place) { if let Some(store) = crate::roster::store(&place.settings()) { \
            let _ = crate::roster::withdraw(store.as_ref()); } }"), "{}\n{files}", update(REAL));
        let forget = function(&sharing, "sharing.rs", "impl Forget for crate::roster::Syncer {", REAL);
        assert!(has(forget, "fn forget(&mut self) { self.reset(); } }"), "{}\n{forget}", update(REAL));
    }
}
