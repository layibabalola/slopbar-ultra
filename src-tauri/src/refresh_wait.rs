//! Waiting for the refresh lock (`AppState::refresh_lock`), always within a limit.
//!
//! A refresh holds the lock while it runs. In builds that wait for one before an update's
//! installer ends SlopBar, the handover keeps the lock until its thread ends
//! (`update_check::hand_over`), which can be when SlopBar restarts. A lock held that long may leave
//! the numbers stale, and nothing else:
//! - a refresh that someone asks for waits for the lock `LOCK_WAIT` at most (`under`). Then it
//!   gives up and its caller gets the last numbers (`app::do_refresh`), and a refresh follows once
//!   the lock is free (`app::refresh_soon`, which waits for it holding nothing, with nobody waiting
//!   for it);
//! - nothing waits for the lock while it holds what others need. A settings change lets go of
//!   `settings_lock` before its refresh (`app::store_settings`); an account's reservation ends
//!   before any refresh (`app::disconnect_one`, `app::start_add_account`); and the backoff that a
//!   sign-in or a disconnect clears is left to the next refresh, which clears it under the lock
//!   (`app::retry_soon`), instead of waiting for the lock;
//! - a door call whose refresh gave up says so in its answer (`noting_waits`, `mark_waiting`), and
//!   the door lets a connection go once its client has given up (`door::CLIENT_TIMEOUT`), so its
//!   slots stay free for get_app_info, restart_app, install_update and quit_app.

use std::cell::Cell;
use std::future::Future;
use std::time::Duration;

/// How long anything that answers someone waits for the refresh lock. A refresh normally ends well
/// inside it, and a door call that waits this long and then makes one request of its own (20 s at
/// most, `app::http_client`) still answers before its client gives up (`door::CLIENT_TIMEOUT`).
pub const LOCK_WAIT: Duration = Duration::from_secs(30);

/// The field a door answer gets when a refresh it needed gave up waiting (`mark_waiting`)...
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

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn a_door_call_that_waited_still_answers_before_its_client_gives_up() {
        let app = code(include_str!("app.rs"));
        let client = body(&app, "fn http_client(");
        assert!(client.contains(".timeout(Duration::from_secs(20))"), "{client}");
        assert!(LOCK_WAIT + Duration::from_secs(20) < crate::door::CLIENT_TIMEOUT, "the wait and one request of its own fit in the door's time");
        assert!(LOCK_WAIT >= Duration::from_secs(30), "a refresh normally ends well inside it");
    }

    /// `source` without whole-line comments, so a line commented out does not count.
    fn code(source: &str) -> String {
        source.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n")
    }

    /// The function in `code` whose signature starts `signature`, to the first line that is `}`.
    fn body<'a>(code: &'a str, signature: &str) -> &'a str {
        let start = code.find(signature).unwrap_or_else(|| panic!("no `{signature}`"));
        let rest = &code[start..];
        rest.find("\n}").map_or(rest, |end| &rest[..end + 2])
    }

    /// Where `what` first appears in `text`, after byte `from`.
    fn at(text: &str, what: &str, from: usize) -> Option<usize> {
        text[from..].find(what).map(|i| i + from)
    }

    /// Whether `word` stands alone in `line`, not inside a longer name.
    fn mentions(line: &str, word: &str) -> bool {
        let named = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        line.match_indices(word).any(|(at, _)| !named(line[..at].chars().next_back()) && !named(line[at + word.len()..].chars().next()))
    }

    /// app.rs takes the refresh lock in two places only: a refresh, within LOCK_WAIT, and
    /// `refresh_soon`, which holds nothing while it waits and which nobody waits for. Every other
    /// mention is the field and its value.
    #[test]
    fn every_wait_for_the_refresh_lock_is_bounded_or_holds_nothing() {
        let app = code(include_str!("app.rs"));
        let (mut bounded, mut soon) = (0, 0);
        for line in app.lines().filter(|l| mentions(l, "refresh_lock")) {
            let line = line.trim();
            let field = line.trim_start_matches("pub(crate) ").starts_with("refresh_lock: ") && (line.ends_with("Mutex<()>,") || line.ends_with("Mutex<()>>,"));
            let value = line.starts_with("refresh_lock: ") && line.contains("tokio::sync::Mutex::new(())");
            if line.starts_with("let refreshed = refresh_wait::under(&state.refresh_lock, refresh_wait::LOCK_WAIT, ") {
                bounded += 1;
            } else if line == "drop(app.state::<AppState>().refresh_lock.lock().await);" {
                soon += 1;
            } else {
                assert!(field || value, "an unbounded wait for the refresh lock, or one that holds something: {line}");
            }
        }
        assert_eq!((bounded, soon), (1, 1));
        let within = body(&app, "async fn refresh_within(");
        assert!(within.contains("refresh_wait::under(&state.refresh_lock, refresh_wait::LOCK_WAIT, || refresh_locked(app))"), "{within}");
        assert!(within.contains("if refreshed.is_none() {\n        refresh_soon(app);\n    }"), "a refresh that gave up is followed by one once the lock is free:\n{within}");
        let soon = body(&app, "fn refresh_soon(");
        assert!(soon.contains("tauri::async_runtime::spawn(async move {"), "nobody waits for it:\n{soon}");
        assert!(body(&app, "async fn do_refresh(").contains("refresh_within(app).await"));
        let locked = body(&app, "async fn refresh_locked(");
        assert!(locked.contains("state.retry_due.lock().unwrap().drain()") && locked.contains("providers::retry_now("), "it clears the backoffs left to it:\n{locked}");
    }

    /// `settings_lock` is never held while anything waits for a refresh: `store_settings` takes the
    /// lock's guard and lets go of it, and never refreshes; nothing refreshes between taking the
    /// lock and handing it to `store_settings`.
    #[test]
    fn nothing_waits_for_a_refresh_while_holding_settings_lock() {
        let app = code(include_str!("app.rs"));
        let refreshes = ["do_refresh(", "refresh_within(", "refresh_soon(", "refresh_locked("];
        let store = body(&app, "async fn store_settings(");
        assert!(store.starts_with("async fn store_settings(app: &AppHandle, held: tokio::sync::MutexGuard<'_, ()>, settings: Settings) -> Result<bool, String> {"), "{store}");
        assert!(store.contains("drop(held);"), "{store}");
        for call in refreshes {
            assert!(!store.contains(call), "store_settings refreshes (`{call}`) while its caller holds settings_lock:\n{store}");
        }
        let mut taken = 0;
        let mut from = 0;
        while let Some(lock) = at(&app, "settings_lock.lock().await", from) {
            taken += 1;
            from = lock + 1;
            let store = at(&app, "store_settings(", lock).expect("each settings change ends in store_settings");
            let held = &app[lock..store];
            for call in refreshes {
                assert!(!held.contains(call), "a refresh (`{call}`) while settings_lock is held:\n{held}");
            }
            assert!(app[store..].starts_with("store_settings(&app, held, ") || app[store..].starts_with("store_settings(app, held, "), "the lock goes with the change:\n{}", &app[lock..store + 40]);
        }
        assert_eq!(taken, 6, "save_settings, update_settings, apply_backup, remove_one, set_project_pin, the compact toggle");
    }

    /// An account's reservation ends before anything waits for a refresh, and a door call that
    /// waited for one says so.
    #[test]
    fn reservations_end_before_a_refresh_and_door_calls_say_when_one_waited() {
        let app = code(include_str!("app.rs"));
        let disconnect = body(&app, "async fn disconnect_one(");
        let dropped = disconnect.find("drop(operation);").expect("the reservation ends");
        assert!(dropped < disconnect.find("retry_soon(app, ").expect("its backoff is left to the next refresh"), "{disconnect}");
        let add = body(&app, "async fn start_add_account(");
        let dropped = add.find("drop(pending);").expect("the reservation ends");
        assert!(dropped > add.find("job.wait(").unwrap() && dropped < add.find("do_refresh(&app).await").unwrap(), "it covers the sign-in, not the refresh:\n{add}");
        let login = body(&app, "async fn start_monitoring_login(");
        assert!(login.contains("retry_soon(&app, key);") && login.contains("refresh_soon(&app);"), "{login}");

        assert!(app.contains("door::serve(door::pipe_name(), move |request| door_reply(h.clone(), request))"), "the door answers through door_reply");
        let reply = body(&app, "async fn door_reply(");
        assert!(reply.contains("refresh_wait::noting_waits(door_call(app, request))") && reply.contains("refresh_wait::mark_waiting"), "{reply}");
        let switch = body(&app, "async fn agent_switch(");
        assert!(switch.contains("let refreshed = refresh_within(app).await;") && switch.contains("result[\"switched_to_requested\"] = held.into();"),
            "whether the switch held is unknown (null) when its refresh waited:\n{switch}");
    }
}
