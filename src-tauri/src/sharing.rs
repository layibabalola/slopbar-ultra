//! Where in-use status is shared, and this machine's files there kept in step with the settings,
//! whatever order settings saves, refreshes and roster syncs run in.
//!
//! A save that moves sharing (another folder or rclone remote) or turns the roster off leaves this
//! machine's presence record and roster at the old place. The save only notes them (`Sharing::
//! saved`): it holds `settings_lock` and must not wait on a folder or rclone, and a refresh that
//! read the settings before it could still write to the old place afterwards. The next refresh
//! withdraws them, under the refresh lock, before it reads the settings and writes anything
//! (`Sharing::catch_up`), so a write made meanwhile for the old place is withdrawn after it.
//!
//! Every write first checks that the live settings still share where it was meant for: a refresh's
//! presence record just before it is written (`still`), and a roster sync while it holds the
//! roster's lock (`Sharing::sync_roster`), which the withdrawals also hold, so they come before or
//! after a sync as a whole. A roster sync notes where it synced last: a sync at another place
//! starts afresh, so a move never leaves the new place without this machine's roster.
//!
//! What a save noted stays due until a withdrawal of it ends in time:
//! - each withdrawal runs on a thread of its own, all of them side by side, and a refresh waits
//!   `WITHDRAW_LIMIT` at most for them and for the roster's lock, so a folder that does not answer
//!   holds the refresh lock that long at most. One still running then stays due: it is not started
//!   again while it runs, and once it has ended the next refresh withdraws there again, in order
//!   with the writes. Nothing writes to a place sharing has left, so a withdrawal that ends late
//!   harms nothing there; where sharing has come back, the refresh after it writes again;
//! - SlopBar's exit withdraws what is due as a refresh would (`Sharing::exit`), so an exit between
//!   a save and its refresh (a refresh still running, or waiting for the refresh lock) loses
//!   nothing;
//! - and what is due is kept in a file (`Sharing::kept_in`), so neither a crash, nor an update that
//!   ends SlopBar without its exit, nor an exit that ran out of time loses it either: the first
//!   refresh after the next start withdraws it. Otherwise another computer would honour a roster
//!   left at the old place for 60 days.
//!
//! So after any sequence of saves, in the UI or through the door and at any speed, this machine's
//! presence and roster files at every place, and the roster sync, end as the last save says.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::settings::Settings;

/// How long a refresh, or SlopBar's exit, waits for the withdrawals saves left, all of them side by
/// side, and for the roster's lock they take. A withdrawal normally takes well under a second at a
/// synced folder and a few seconds at an rclone remote (a presence record there is two rclone calls,
/// each ended at 20 s by `rclone`), so only a place that does not answer reaches it. It is a third
/// of the time a door call waits for the refresh lock (`refresh_wait::LOCK_WAIT`), so a refresh
/// that meets such a place still answers it, and it fits inside the exit's limit for a part
/// (`update_check::EXIT_PART_LIMIT`). A withdrawal still running then stays due, and is not lost.
pub const WITHDRAW_LIMIT: Duration = Duration::from_secs(10);

/// The file in the app's folder that keeps what is due between runs (`Sharing::kept_in`).
pub const DUE_FILE: &str = "sharing-due.json";

/// Where a save shares in-use status: the synced folder or rclone remote, and whether the
/// account roster is on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub folder: Option<String>,
    pub remote: Option<String>,
    pub roster: bool,
}

impl Place {
    pub fn of(settings: &Settings) -> Self {
        Place { folder: settings.presence_folder.clone(), remote: settings.presence_remote.clone(), roster: settings.account_roster }
    }

    /// The same folder or remote, whether or not the roster is on.
    pub fn same_location(&self, other: &Place) -> bool {
        self.folder == other.folder && self.remote == other.remote
    }

    fn shares(&self) -> bool {
        self.folder.is_some() || self.remote.is_some()
    }

    /// Settings that open this place's stores (`presence::store`, `roster::store`).
    pub fn settings(&self) -> Settings {
        Settings { presence_folder: self.folder.clone(), presence_remote: self.remote.clone(), account_roster: self.roster, ..Settings::default() }
    }
}

/// Whether a refresh's presence record, made for `copy`, may still be written: the live settings
/// share at the same place.
pub fn still(copy: &Place, live: &Place) -> bool {
    copy.same_location(live)
}

/// What saves leave for the next refresh (see the module docs).
#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Due {
    /// Places to withdraw this machine's presence record from.
    pub presence: Vec<Place>,
    /// Places to withdraw this machine's roster from.
    pub roster: Vec<Place>,
    /// Forget what the roster sync knew: its keys, what it wrote and what it read. Not kept in the
    /// file: the roster sync starts afresh with SlopBar anyway.
    #[serde(skip)]
    pub reset: bool,
}

impl Due {
    fn note(&mut self, before: &Place, after: &Place) {
        let moved = !before.same_location(after);
        if moved && before.shares() && !self.presence.contains(before) {
            self.presence.push(before.clone());
        }
        if moved || before.roster != after.roster {
            if before.roster && before.shares() && !self.roster.contains(before) {
                self.roster.push(before.clone());
            }
            self.reset = true;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.presence.is_empty() && self.roster.is_empty() && !self.reset
    }

    /// Every withdrawal due, presence records first.
    fn withdrawals(&self) -> Vec<(Kind, Place)> {
        let presence = self.presence.iter().map(|p| (Kind::Presence, p.clone()));
        presence.chain(self.roster.iter().map(|p| (Kind::Roster, p.clone()))).collect()
    }

    /// One withdrawal made: it is no longer due.
    fn made(&mut self, kind: Kind, place: &Place) {
        let list = match kind {
            Kind::Presence => &mut self.presence,
            Kind::Roster => &mut self.roster,
        };
        if let Some(at) = list.iter().position(|p| p == place) {
            list.remove(at);
        }
    }
}

/// Withdraws this machine's files from a place sharing left. Each withdrawal runs on a thread of
/// its own (`Sharing::catch_up`), with a copy of this.
pub trait Withdraw: Clone + Send + 'static {
    fn presence(&self, place: &Place);
    fn roster(&self, place: &Place);
}

/// The real files: the presence record (`presence::withdraw`, which says this machine has nothing
/// signed in) and the roster (`roster::withdraw`, which removes it).
#[derive(Clone)]
pub struct Files {
    /// The refresh interval, in seconds, that a presence record states.
    pub interval: u64,
}

impl Withdraw for Files {
    fn presence(&self, place: &Place) {
        if let Some(store) = crate::presence::store(&place.settings()) {
            crate::presence::withdraw(store.as_ref(), self.interval);
        }
    }

    fn roster(&self, place: &Place) {
        if let Some(store) = crate::roster::store(&place.settings()) {
            let _ = crate::roster::withdraw(store.as_ref());
        }
    }
}

/// What a roster sync keeps between runs, that `Sharing` forgets after a move (`roster::Syncer`).
pub trait Forget {
    fn forget(&mut self);
}

impl Forget for crate::roster::Syncer {
    fn forget(&mut self) {
        self.reset();
    }
}

/// Which of this machine's files a withdrawal removes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Presence,
    Roster,
}

/// A withdrawal still running past its limit (a place that does not answer), and whether it has
/// ended since.
struct Late {
    kind: Kind,
    place: Place,
    ended: Arc<AtomicBool>,
}

/// The roster's sync and the place it last synced with, and the withdrawals still running late.
#[derive(Default)]
pub struct RosterSync<S> {
    pub syncer: S,
    place: Option<Place>,
    late: Vec<Late>,
}

/// What saves left for the next refresh, and the account roster's sync, which share one lock with
/// the withdrawals (see the module docs).
pub struct Sharing<S = crate::roster::Syncer> {
    due: Mutex<Due>,
    roster: Mutex<RosterSync<S>>,
    /// Where what is due is kept between runs (`kept_in`).
    file: Option<PathBuf>,
}

impl<S: Default> Default for Sharing<S> {
    fn default() -> Self {
        Sharing { due: Mutex::default(), roster: Mutex::new(RosterSync { syncer: S::default(), place: None, late: vec![] }), file: None }
    }
}

impl<S: Default> Sharing<S> {
    /// Sharing that keeps what is due in `file`, starting with what an earlier run left there:
    /// a crash, an update that ended SlopBar without its exit, or an exit that ran out of time.
    pub fn kept_in(file: PathBuf) -> Self {
        let due = std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<Due>(&bytes).ok()).unwrap_or_default();
        Sharing { due: Mutex::new(due), file: Some(file), ..Sharing::default() }
    }
}

/// A lock that a panic elsewhere does not poison for us: what it guards is never left half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `mutex`'s lock, waited for until `deadline` at most. std's mutex has no timed lock, so this tries
/// again every few milliseconds.
fn lock_by<T>(mutex: &Mutex<T>, deadline: Instant) -> Option<MutexGuard<'_, T>> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) if Instant::now() >= deadline => return None,
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Says from a withdrawal's thread that it ended, whether it returned or panicked.
struct Ends {
    ended: Arc<AtomicBool>,
    index: usize,
    done: mpsc::Sender<usize>,
}

impl Drop for Ends {
    fn drop(&mut self) {
        self.ended.store(true, Ordering::SeqCst);
        let _ = self.done.send(self.index);
    }
}

/// Starts one withdrawal on a thread of its own, which sends `index` on `done` once it has ended and
/// sets the flag returned. None when no thread could be started.
fn start(files: &impl Withdraw, kind: Kind, place: &Place, index: usize, done: &mpsc::Sender<usize>) -> Option<Arc<AtomicBool>> {
    let ended = Arc::new(AtomicBool::new(false));
    let (files, place, flag, done) = (files.clone(), place.clone(), ended.clone(), done.clone());
    let spawned = std::thread::Builder::new().name("sharing-withdrawal".into()).spawn(move || {
        let _ends = Ends { ended: flag, index, done };
        match kind {
            Kind::Presence => files.presence(&place),
            Kind::Roster => files.roster(&place),
        }
    });
    spawned.ok().map(|_| ended)
}

impl<S: Forget> Sharing<S> {
    /// A settings save, called holding `settings_lock` (no I/O but this machine's own disk): notes
    /// what the next refresh withdraws, and keeps it in the file. Whether a refresh is due: the
    /// providers polled, where in-use status is shared, or whether the roster is on, changed.
    pub fn saved(&self, before: &Settings, after: &Settings) -> bool {
        let (from, to) = (Place::of(before), Place::of(after));
        {
            let mut due = lock(&self.due);
            let noted = due.presence.len() + due.roster.len();
            due.note(&from, &to);
            if due.presence.len() + due.roster.len() != noted {
                self.keep(&due);
            }
        }
        before.providers.claude != after.providers.claude || before.providers.codex != after.providers.codex || from != to
    }

    /// Whether a save left anything for `catch_up`.
    pub fn is_due(&self) -> bool {
        !lock(&self.due).is_empty()
    }

    /// A refresh's first step, under the refresh lock and before it reads the settings (on a
    /// thread that may wait on folders and rclone): withdraws what saves left behind, each on a
    /// thread of its own, and forgets what the roster sync knew if they asked for that or a roster
    /// was withdrawn, holding the roster's lock throughout; all within `limit` (`WITHDRAW_LIMIT`).
    /// Whether the roster sync was reset, so the caller clears what it shows of it.
    pub fn catch_up(&self, files: &impl Withdraw, limit: Duration) -> bool {
        self.catch_up_by(files, Instant::now() + limit)
    }

    /// SlopBar's exit (`app::on_exit`): this machine's presence withdrawn where the settings share
    /// now (`live`), so the other computers stop showing its accounts as in use, and what saves left
    /// that no refresh has withdrawn yet, as a refresh would (`catch_up`): all side by side, within
    /// `limit`. What does not end in time stays in the file, for the next start.
    pub fn exit(&self, live: &Place, files: &impl Withdraw, limit: Duration) {
        let deadline = Instant::now() + limit;
        let (done, ended) = mpsc::channel();
        let here = if live.shares() { start(files, Kind::Presence, live, 0, &done) } else { None };
        drop(done);
        self.catch_up_by(files, deadline);
        if here.is_some() {
            let _ = ended.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }

    fn catch_up_by(&self, files: &impl Withdraw, deadline: Instant) -> bool {
        // A roster sync that hangs on a folder holds the roster's lock: waited for until the
        // deadline at most, and then everything stays due.
        let Some(mut roster) = lock_by(&self.roster, deadline) else { return false };
        let (due, asked) = {
            let mut due = lock(&self.due);
            (due.withdrawals(), std::mem::take(&mut due.reset))
        };
        // One that ran late and has ended since is made again below, in order with the writes.
        roster.late.retain(|late| !late.ended.load(Ordering::SeqCst));
        let (done, ended) = mpsc::channel();
        let mut started = Vec::new();
        for (index, (kind, place)) in due.iter().enumerate() {
            if roster.late.iter().any(|late| late.kind == *kind && late.place.same_location(place)) {
                continue;
            }
            if let Some(flag) = start(files, *kind, place, index, &done) {
                started.push((index, flag));
            }
        }
        drop(done);
        let mut made = Vec::new();
        while made.len() < started.len() {
            match ended.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(index) => made.push(index),
                Err(_) => break,
            }
        }
        for (index, flag) in started {
            if !made.contains(&index) {
                let (kind, place) = due[index].clone();
                roster.late.push(Late { kind, place, ended: flag });
            }
        }
        // A roster withdrawn where sharing has come back is written there again by the next sync.
        let reset = asked || made.iter().any(|&index| due[index].0 == Kind::Roster);
        if reset {
            roster.syncer.forget();
            roster.place = None;
        }
        if !made.is_empty() {
            let mut left = lock(&self.due);
            for &index in &made {
                left.made(due[index].0, &due[index].1);
            }
            self.keep(&left);
        }
        reset
    }

    /// Keeps what is due in the file, on this machine's own disk; removes the file once nothing is.
    fn keep(&self, due: &Due) {
        let Some(file) = &self.file else { return };
        if due.presence.is_empty() && due.roster.is_empty() {
            let _ = std::fs::remove_file(file);
        } else if let Ok(bytes) = serde_json::to_vec(due) {
            let _ = crate::private_store::write(file, &bytes);
        }
    }

    /// A roster sync for `copy` (on a thread that may wait on folders and rclone): runs `sync`
    /// holding the roster's lock, only while the live settings (`live`, read holding it) still
    /// share at that place with the roster on, and afresh if the last sync was elsewhere.
    pub fn sync_roster<R>(&self, copy: &Place, live: impl FnOnce() -> Place, sync: impl FnOnce(&mut S) -> R) -> Option<R> {
        let mut roster = lock(&self.roster);
        let live = live();
        if !(copy.same_location(&live) && live.roster) {
            return None;
        }
        if roster.place.as_ref().is_some_and(|p| !p.same_location(copy)) {
            roster.syncer.forget();
        }
        roster.place = Some(copy.clone());
        Some(sync(&mut roster.syncer))
    }

    /// The roster's sync, for what does not depend on the place (a Retry's news, a passphrase).
    /// It waits for the roster's lock, which a sync that hangs on a folder holds: call it off the
    /// runtime's threads (`spawn_blocking`).
    pub fn with_syncer<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        f(&mut lock(&self.roster).syncer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
    use std::sync::Condvar;

    /// How long a test waits for what should happen at once (a withdrawal at a place that answers).
    /// The wait ends as soon as it happens, so only a starved run waits at all, and none this long.
    const PATIENT: Duration = Duration::from_secs(5 * 60);
    /// A place that does not answer starts answering by itself after this. A limit in the code ends
    /// a wait far sooner, however starved the run; with the limit gone, the wait ends here and the
    /// test fails, rather than hanging.
    const SELF_ENDING: Duration = Duration::from_secs(60);
    /// The limit the tests give a wait for a place that does not answer: far inside SELF_ENDING.
    const SHORT: Duration = Duration::from_millis(100);

    fn at(folder: &str, roster: bool) -> Settings {
        Settings { presence_folder: Some(folder.into()), account_roster: roster, ..Settings::default() }
    }

    /// A roster sync as `roster::Syncer` decides: it writes when it has not since its last reset,
    /// or when this machine's accounts changed, and does not note where it wrote.
    #[derive(Default)]
    struct Syncer {
        written: Option<u32>,
    }

    impl Forget for Syncer {
        fn forget(&mut self) {
            self.written = None;
        }
    }

    /// This machine's files at each place, and every write with where the settings pointed then.
    #[derive(Default)]
    struct World {
        /// "in use" (a record naming this machine's accounts) or "withdrawn" (an empty one).
        presence: BTreeMap<String, &'static str>,
        roster: BTreeSet<String>,
        /// (the place a write was made for, where the live settings shared when it was made)
        writes: Vec<(String, String)>,
        /// Every withdrawal, in the order they were made ("presence A", "roster A").
        withdrawals: Vec<String>,
        /// Every withdrawal started, made or not.
        attempts: Vec<String>,
        /// How often the app cleared the roster news it shows (`roster_seen`).
        news_cleared: u32,
    }

    fn name(place: &Place) -> String {
        place.folder.clone().unwrap_or_default()
    }

    /// The places that do not answer: anything done there waits until the test lets it go
    /// (`answer`), or until SELF_ENDING.
    #[derive(Default)]
    struct Hung {
        places: Mutex<BTreeSet<String>>,
        answered: Condvar,
    }

    impl Hung {
        fn hang(&self, place: &str) {
            self.places.lock().unwrap().insert(place.into());
        }

        fn answer(&self, place: &str) {
            self.places.lock().unwrap().remove(place);
            self.answered.notify_all();
        }

        /// Waits while `place` does not answer; whether it waited until SELF_ENDING.
        fn wait(&self, place: &str) -> bool {
            let deadline = Instant::now() + SELF_ENDING;
            let mut places = self.places.lock().unwrap();
            while places.contains(place) {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return true;
                }
                places = self.answered.wait_timeout(places, left).unwrap().0;
            }
            false
        }
    }

    /// The withdrawals, as `Files` makes them, in the test's world.
    struct Io {
        world: Arc<Mutex<World>>,
        hung: Arc<Hung>,
        /// Set when a withdrawal waited on a place until SELF_ENDING: then no limit ended it.
        self_ended: Arc<AtomicBool>,
        /// Every withdrawal started, counted as `catch_up` starts it: it takes a copy of this
        /// (`start`) before its thread runs, however late a starved run schedules that thread.
        started: Arc<AtomicUsize>,
    }

    impl Clone for Io {
        fn clone(&self) -> Self {
            self.started.fetch_add(1, SeqCst);
            self.shared()
        }
    }

    impl Io {
        fn new() -> Self {
            Io { world: Arc::default(), hung: Arc::default(), self_ended: Arc::default(), started: Arc::default() }
        }

        /// The same world, without counting a withdrawal.
        fn shared(&self) -> Self {
            Io { world: self.world.clone(), hung: self.hung.clone(), self_ended: self.self_ended.clone(), started: self.started.clone() }
        }

        fn reach(&self, what: String, place: &Place) -> MutexGuard<'_, World> {
            self.world.lock().unwrap().attempts.push(what);
            if self.hung.wait(&name(place)) {
                self.self_ended.store(true, SeqCst);
            }
            self.world.lock().unwrap()
        }
    }

    impl Withdraw for Io {
        fn presence(&self, place: &Place) {
            let what = format!("presence {}", name(place));
            let mut world = self.reach(what.clone(), place);
            world.presence.insert(name(place), "withdrawn");
            world.withdrawals.push(what);
        }

        fn roster(&self, place: &Place) {
            let what = format!("roster {}", name(place));
            let mut world = self.reach(what.clone(), place);
            world.roster.remove(&name(place));
            world.withdrawals.push(what);
        }
    }

    /// The app, as far as sharing goes: the live settings, `Sharing`, the files, and a count that
    /// stands for this machine's accounts.
    struct App {
        settings: Mutex<Settings>,
        sharing: Sharing<Syncer>,
        io: Io,
        accounts: AtomicU32,
    }

    impl App {
        fn new(settings: Settings) -> Self {
            App { settings: Mutex::new(settings), sharing: Sharing::default(), io: Io::new(), accounts: AtomicU32::new(1) }
        }

        /// The same computer started again, keeping what is due in `file`.
        fn started(&self, file: PathBuf) -> Self {
            let settings = Mutex::new(self.settings.lock().unwrap().clone());
            App { settings, sharing: Sharing::kept_in(file), io: self.io.shared(), accounts: AtomicU32::new(self.accounts.load(SeqCst)) }
        }

        fn world(&self) -> MutexGuard<'_, World> {
            self.io.world.lock().unwrap()
        }

        fn live(&self) -> Place {
            Place::of(&self.settings.lock().unwrap())
        }

        /// `app::store_settings`: the settings replaced, then `saved`. Whether a refresh is due.
        fn save(&self, after: Settings) -> bool {
            let before = std::mem::replace(&mut *self.settings.lock().unwrap(), after.clone());
            self.sharing.saved(&before, &after)
        }

        /// `app::refresh_locked`'s start: `catch_up` when `is_due` says a save left something,
        /// clearing the roster news the app shows when it reset the roster sync; then the settings
        /// read.
        fn start(&self) -> Place {
            self.start_within(PATIENT)
        }

        fn start_within(&self, limit: Duration) -> Place {
            if self.sharing.is_due() && self.sharing.catch_up(&self.io, limit) {
                self.world().news_cleared += 1;
            }
            self.live()
        }

        /// The rest of a refresh that read `copy`: its presence record, written while `still`
        /// holds, then its roster sync (`app::sync_roster`).
        fn finish(&self, copy: &Place) {
            if still(copy, &self.live()) {
                let live = name(&self.live());
                let mut world = self.world();
                world.presence.insert(name(copy), "in use");
                world.writes.push((name(copy), live));
            }
            self.sync(copy);
        }

        /// A roster sync for `copy`: a refresh's, or an agent's (`get_roster` with sync). At a place
        /// that does not answer, it waits there holding the roster's lock, as a real sync would.
        fn sync(&self, copy: &Place) {
            let accounts = self.accounts.load(SeqCst);
            self.sharing.sync_roster(copy, || self.live(), |syncer| {
                if syncer.written != Some(accounts) {
                    syncer.written = Some(accounts);
                    self.io.hung.wait(&name(copy));
                    let live = name(&self.live());
                    let mut world = self.world();
                    world.roster.insert(name(copy));
                    world.writes.push((name(copy), live));
                }
            });
        }

        fn refresh(&self) {
            let copy = self.start();
            self.finish(&copy);
        }

        fn refresh_within(&self, limit: Duration) {
            let copy = self.start_within(limit);
            self.finish(&copy);
        }

        /// `app::on_exit`'s part for sharing.
        fn exit(&self, limit: Duration) {
            self.sharing.exit(&self.live(), &self.io, limit);
        }
    }

    /// Waits up to PATIENT for `check` to hold.
    fn eventually(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + PATIENT;
        while !check() {
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Two saves within one refresh (the Sharing dropdown arrowed through: it saves at every
    /// step). The refresh that read the first place writes nothing there once the settings moved
    /// on, though its accounts changed meanwhile; an agent's roster sync then writes at the last
    /// place at once; and the refresh the saves asked for withdraws what was left at every place
    /// sharing left, so only the last one keeps this machine's presence and roster.
    #[test]
    fn two_saves_within_one_refresh_leave_only_the_last_place() {
        let app = App::new(at("A", true));
        app.refresh();
        {
            let world = app.world();
            assert_eq!((world.presence["A"], world.roster.clone()), ("in use", BTreeSet::from(["A".to_string()])), "sharing at A");
        }

        let copy = app.start();
        app.accounts.fetch_add(1, SeqCst);
        assert!(app.save(at("B", true)), "a save asks for a refresh");
        assert!(app.save(at("C", true)), "and so does the next");
        app.finish(&copy);
        app.sync(&app.live());
        assert!(app.world().roster.contains("C"), "an agent's sync after the move writes at once");
        app.refresh();

        let world = app.world();
        assert_eq!(world.presence, BTreeMap::from([("A".into(), "withdrawn"), ("B".into(), "withdrawn"), ("C".into(), "in use")]), "nothing in use left at A or B");
        assert_eq!(world.roster, BTreeSet::from(["C".to_string()]), "the roster only at C");
        for (made_for, live) in &world.writes {
            assert_eq!(made_for, live, "a write for a place the settings had left: {:?}", world.writes);
        }
    }

    /// After a move, a roster sync at the new place writes there at once, though this machine's
    /// accounts did not change: the sync notes where it wrote last, which `roster::Syncer` does not.
    #[test]
    fn a_roster_sync_after_a_move_writes_at_the_new_place_at_once() {
        let app = App::new(at("A", true));
        app.refresh();
        assert!(app.save(at("C", true)));
        app.sync(&app.live());
        assert_eq!(app.world().roster, BTreeSet::from(["A".to_string(), "C".to_string()]), "C at once; A until the next refresh");
        app.refresh();
        assert_eq!(app.world().roster, BTreeSet::from(["C".to_string()]));
    }

    /// Turning the roster off, then on again, within one refresh: it is withdrawn and written
    /// again, never left withdrawn while the settings have it on. Turned off for good, it goes.
    #[test]
    fn the_roster_turned_off_and_on_ends_as_the_last_save_says() {
        let app = App::new(at("A", true));
        app.refresh();
        let copy = app.start();
        assert!(app.save(at("A", false)) && app.save(at("A", true)), "each asks for a refresh");
        app.finish(&copy);
        app.refresh();
        assert_eq!(app.world().roster, BTreeSet::from(["A".to_string()]), "written again after the withdrawal");

        assert!(app.save(at("A", false)), "turning the roster off asks for a refresh, which withdraws it");
        app.refresh();
        let world = app.world();
        assert!(world.roster.is_empty());
        assert_eq!(world.presence["A"], "in use", "presence stays: sharing did not move");
    }

    #[test]
    fn a_save_notes_only_what_it_moves_and_says_when_a_refresh_is_due() {
        let sharing: Sharing<Syncer> = Sharing::default();
        let due = || std::mem::take(&mut *lock(&sharing.due));
        let mut other = at("A", true);
        other.compact_flyout = !other.compact_flyout;
        assert!(!sharing.saved(&at("A", true), &other), "nothing a refresh reads");
        let mut codex = at("A", true);
        codex.providers.codex = !codex.providers.codex;
        assert!(sharing.saved(&at("A", true), &codex), "a provider turned on or off");
        let mut claude = at("A", true);
        claude.providers.claude = !claude.providers.claude;
        assert!(sharing.saved(&at("A", true), &claude));
        assert!(!sharing.is_due(), "and nothing to withdraw or forget");

        assert!(sharing.saved(&Settings::default(), &at("A", false)), "sharing turned on");
        assert_eq!(due(), Due { presence: vec![], roster: vec![], reset: true }, "nothing was shared before; the roster sync starts afresh");
        assert!(sharing.saved(&at("A", false), &at("B", false)), "moved, with the roster off");
        assert_eq!(due(), Due { presence: vec![Place::of(&at("A", false))], roster: vec![], reset: true });
        assert!(sharing.saved(&at("A", true), &at("B", true)), "moved, with the roster on");
        assert_eq!(due(), Due { presence: vec![Place::of(&at("A", true))], roster: vec![Place::of(&at("A", true))], reset: true });
        assert!(sharing.saved(&at("A", true), &Settings::default()), "sharing turned off");
        assert_eq!(due(), Due { presence: vec![Place::of(&at("A", true))], roster: vec![Place::of(&at("A", true))], reset: true });
        assert!(sharing.saved(&at("A", false), &at("A", true)) && sharing.saved(&at("A", true), &at("A", false)), "the roster on, then off");
        assert_eq!(due(), Due { presence: vec![], roster: vec![Place::of(&at("A", true))], reset: true }, "withdrawn where it was on; presence stays");
    }

    /// A place left twice (A, B, A, B) is withdrawn once, and the last save is where the refresh
    /// writes.
    #[test]
    fn a_place_left_twice_is_withdrawn_once_and_written_last() {
        let app = App::new(at("A", true));
        app.refresh();
        for folder in ["B", "A", "B"] {
            app.save(at(folder, true));
        }
        {
            let due = lock(&app.sharing.due);
            assert_eq!((due.presence.len(), due.roster.len(), due.reset), (2, 2, true), "{due:?}");
        }
        app.refresh();
        let world = app.world();
        assert_eq!(world.presence, BTreeMap::from([("A".into(), "withdrawn"), ("B".into(), "in use")]));
        assert_eq!(world.roster, BTreeSet::from(["B".to_string()]));
    }

    /// What the refresh does with a save, through the app's own gate (`is_due`): the roster turned
    /// off where it was, then a move, each leaves something due; the next refresh withdraws each
    /// file once and clears the roster news shown, and leaves nothing due, so the refresh after it
    /// withdraws nothing again.
    #[test]
    fn a_refresh_takes_what_a_save_left_once() {
        let app = App::new(at("A", true));
        app.refresh();
        assert!(!app.sharing.is_due(), "nothing saved, nothing due");
        let after = |app: &App| {
            let world = app.world();
            (world.withdrawals.clone(), world.news_cleared)
        };

        assert!(app.save(at("A", false)), "the roster turned off where it was");
        assert!(app.sharing.is_due(), "is due, though presence stays");
        app.refresh();
        assert!(!app.sharing.is_due(), "nothing left due");
        app.refresh();
        assert_eq!(after(&app), (vec!["roster A".to_string()], 1), "withdrawn once; the roster news cleared");

        assert!(app.save(at("B", true)), "a move");
        assert!(app.sharing.is_due());
        app.refresh();
        assert!(!app.sharing.is_due());
        app.refresh();
        assert_eq!(after(&app), (vec!["roster A".to_string(), "presence A".to_string()], 2), "A's presence once (its roster was gone)");
        let world = app.world();
        assert_eq!((world.presence["B"], world.roster.clone()), ("in use", BTreeSet::from(["B".to_string()])), "sharing at B");
    }

    /// A folder that sharing just left does not answer. The refresh waits for its withdrawals for
    /// their limit at most (with the limit gone, it would wait until the folder answers by itself,
    /// and the test fails), and keeps them due; the next refresh does not start them again while
    /// they run. Once the folder answers they end, late, and the refresh after that withdraws there
    /// again, in order with the writes, and clears what is due.
    #[test]
    fn a_folder_that_does_not_answer_holds_a_refresh_for_its_limit_at_most() {
        let app = App::new(at("A", true));
        app.refresh();
        app.io.hung.hang("A");
        assert!(app.save(at("B", true)));
        app.refresh_within(SHORT);
        {
            let world = app.world();
            assert!(!app.io.self_ended.load(SeqCst) && world.withdrawals.is_empty(), "the limit ended the wait, not the folder: {:?}", world.withdrawals);
            assert_eq!((world.presence["B"], world.roster.contains("B")), ("in use", true), "the refresh went on to share at B");
            assert_eq!(world.news_cleared, 1, "the roster sync started afresh");
        }
        assert_eq!(app.io.started.load(SeqCst), 2, "A's presence and roster withdrawals, started");
        assert!(app.sharing.is_due(), "what A still has stays due");
        app.refresh_within(SHORT);
        assert_eq!(app.io.started.load(SeqCst), 2, "not started again while they run");

        app.io.hung.answer("A");
        eventually("the late withdrawals end once A answers", || app.world().withdrawals.len() == 2);
        app.refresh();
        assert_eq!(app.io.started.load(SeqCst), 4, "made again once ended, in order with the writes");
        let world = app.world();
        assert_eq!(world.attempts.len(), 4, "{:?}", world.attempts);
        assert_eq!(world.presence, BTreeMap::from([("A".into(), "withdrawn"), ("B".into(), "in use")]));
        assert_eq!(world.roster, BTreeSet::from(["B".to_string()]), "and B's roster written again after A's withdrawal");
        drop(world);
        assert!(!app.sharing.is_due(), "nothing left due");
    }

    /// A roster sync that hangs at the folder sharing then leaves holds the roster's lock. The
    /// refresh waits for that lock for its limit at most and leaves everything due; once the folder
    /// answers, the sync's late write there is withdrawn after it.
    #[test]
    fn a_sync_that_hangs_holds_a_refresh_for_its_limit_at_most() {
        let app = App::new(at("A", true));
        app.refresh();
        app.io.hung.hang("A");
        app.accounts.fetch_add(1, SeqCst);
        let a = app.live();
        std::thread::scope(|scope| {
            let syncing = scope.spawn(|| app.sync(&a));
            eventually("the sync holds the roster's lock", || app.sharing.roster.try_lock().is_err());
            assert!(app.save(at("B", true)));
            assert!(!app.sharing.catch_up(&app.io, SHORT), "nothing done without the roster's lock");
            assert!(!syncing.is_finished() && app.world().withdrawals.is_empty(), "the limit ended the wait, not the sync");
            assert_eq!(lock(&app.sharing.due).withdrawals().len(), 2, "everything stays due");
            app.io.hung.answer("A");
        });
        assert!(app.world().roster.contains("A"), "the sync wrote at A late, after the move");
        app.refresh();
        let world = app.world();
        assert_eq!(world.roster, BTreeSet::from(["B".to_string()]), "and A's roster is withdrawn after that write");
        assert_eq!(world.presence["A"], "withdrawn");
    }

    /// SlopBar exits between a save and its refresh (a refresh still running, or waiting for the
    /// refresh lock): the exit withdraws what the save left, as the refresh would have. Without
    /// that, another computer would honour this machine's roster left at A for 60 days, and show
    /// its accounts as in use there until they went stale.
    #[test]
    fn an_exit_before_the_refresh_withdraws_what_the_save_left() {
        let app = App::new(at("A", true));
        app.refresh();
        assert!(app.save(at("B", true)), "a move, and no refresh after it");
        app.exit(PATIENT);
        let world = app.world();
        assert!(!world.roster.contains("A"), "the exit left the roster at A: {:?}", world.roster);
        assert_eq!(world.presence["A"], "withdrawn", "the exit left this machine in use at A");
        assert_eq!(world.presence["B"], "withdrawn", "and its presence is withdrawn where sharing is now");
        drop(world);
        assert!(!app.sharing.is_due());
    }

    /// The exit waits for its limit at most, whatever does not answer, and a withdrawal it could not
    /// make is kept in the file: the next start withdraws it. So is one a crash left.
    #[test]
    fn what_an_exit_could_not_withdraw_is_kept_for_the_next_start() {
        let dir = crate::test_support::TempDir::new();
        let file = dir.0.join(DUE_FILE);
        let app = App::new(at("A", true)).started(file.clone());
        app.refresh();
        app.io.hung.hang("A");
        assert!(app.save(at("B", true)));
        assert!(file.exists(), "a save keeps what it left in the file");
        app.exit(SHORT);
        eventually("the exit withdraws presence where sharing is now", || app.world().withdrawals.iter().any(|w| w == "presence B"));
        let withdrawals = app.world().withdrawals.clone();
        assert!(!app.io.self_ended.load(SeqCst) && withdrawals == ["presence B"], "the limit ended the exit, not A: {withdrawals:?}");

        let next = app.started(file.clone());
        drop(app);
        assert!(next.sharing.is_due(), "the next start has what the exit could not withdraw");
        answers(&next, "A", 2);
        next.refresh();
        let world = next.world();
        assert!(!world.roster.contains("A") && world.presence["A"] == "withdrawn", "{:?} {:?}", world.roster, world.presence);
        drop(world);
        assert!(!file.exists(), "nothing kept once nothing is due");

        // A crash: the save, and then no exit at all.
        assert!(next.save(at("C", true)));
        let after_crash = next.started(file.clone());
        drop(next);
        after_crash.refresh();
        let world = after_crash.world();
        assert!(!world.roster.contains("B") && world.presence["B"] == "withdrawn", "{:?} {:?}", world.roster, world.presence);
        drop(world);
        assert!(!file.exists());
    }

    /// Lets `place` answer again, and waits until the `waiting` withdrawals there have ended.
    fn answers(app: &App, place: &str, waiting: usize) {
        app.io.hung.answer(place);
        let at = |list: &[String]| list.iter().filter(|w| w.ends_with(&format!(" {place}"))).count();
        eventually("what waited at the place ended", || at(&app.world().withdrawals) == waiting);
    }

    /// The limit lets a working remote finish, keeps a refresh that meets a place that does not
    /// answer within what a door call waits for it, and fits inside the exit's limit for a part.
    #[test]
    fn the_withdrawal_limit_fits_the_waits_around_it() {
        assert!(WITHDRAW_LIMIT >= Duration::from_secs(10), "two rclone calls that answer in a few seconds");
        assert!(WITHDRAW_LIMIT * 3 <= crate::refresh_wait::LOCK_WAIT);
        assert!(WITHDRAW_LIMIT < crate::update_check::EXIT_PART_LIMIT);
    }

    /// The settings a withdrawal opens a place's stores with (`Place::settings`, in `Files`) share
    /// where that place does: its rclone remote or its folder, with its roster setting.
    #[test]
    fn a_place_opens_its_stores_where_it_shares() {
        let remote = Place { folder: None, remote: Some("dropbox".into()), roster: true };
        let folder = Place { folder: Some(r"C:\Sync\SlopBar".into()), remote: None, roster: false };
        for place in [remote, folder] {
            assert_eq!(Place::of(&place.settings()), place);
        }
    }

    /// The real roster sync, forgotten after a move or with the roster turned off, starts from
    /// nothing: what it found last goes with its keys and what it wrote (`roster::Syncer::reset`).
    #[test]
    fn the_real_roster_sync_forgets_what_it_found() {
        let mut syncer = crate::roster::Syncer::default();
        syncer.seen.wrote = Some(true);
        syncer.seen.unreadable = 2;
        Forget::forget(&mut syncer);
        assert_eq!((syncer.seen.wrote, syncer.seen.unreadable), (None, 0));
    }
}
