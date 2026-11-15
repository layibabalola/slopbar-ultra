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
//! So after any sequence of saves, in the UI or through the door and at any speed, this machine's
//! presence and roster files at every place, and the roster sync, end as the last save says.

use std::sync::{Mutex, MutexGuard};

use crate::settings::Settings;

/// Where a save shares in-use status: the synced folder or rclone remote, and whether the
/// account roster is on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
#[derive(Debug, Default, PartialEq)]
pub struct Due {
    /// Places to withdraw this machine's presence record from.
    pub presence: Vec<Place>,
    /// Places to withdraw this machine's roster from.
    pub roster: Vec<Place>,
    /// Forget what the roster sync knew: its keys, what it wrote and what it read.
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
}

/// Withdraws this machine's files from a place sharing left.
pub trait Withdraw {
    fn presence(&mut self, place: &Place);
    fn roster(&mut self, place: &Place);
}

/// The real files: the presence record (`presence::withdraw`, which says this machine has nothing
/// signed in) and the roster (`roster::withdraw`, which removes it).
pub struct Files {
    /// The refresh interval, in seconds, that a presence record states.
    pub interval: u64,
}

impl Withdraw for Files {
    fn presence(&mut self, place: &Place) {
        if let Some(store) = crate::presence::store(&place.settings()) {
            crate::presence::withdraw(store.as_ref(), self.interval);
        }
    }

    fn roster(&mut self, place: &Place) {
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

/// The roster's sync and the place it last synced with.
#[derive(Default)]
pub struct RosterSync<S> {
    pub syncer: S,
    place: Option<Place>,
}

/// What saves left for the next refresh, and the account roster's sync, which share one lock with
/// the withdrawals (see the module docs).
pub struct Sharing<S = crate::roster::Syncer> {
    due: Mutex<Due>,
    roster: Mutex<RosterSync<S>>,
}

impl<S: Default> Default for Sharing<S> {
    fn default() -> Self {
        Sharing { due: Mutex::default(), roster: Mutex::new(RosterSync { syncer: S::default(), place: None }) }
    }
}

/// A lock that a panic elsewhere does not poison for us: what it guards is never left half-written.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl<S: Forget> Sharing<S> {
    /// A settings save, called holding `settings_lock` (no I/O): notes what the next refresh
    /// withdraws. Whether a refresh is due: the providers polled, where in-use status is shared,
    /// or whether the roster is on, changed.
    pub fn saved(&self, before: &Settings, after: &Settings) -> bool {
        let (from, to) = (Place::of(before), Place::of(after));
        lock(&self.due).note(&from, &to);
        before.providers.claude != after.providers.claude || before.providers.codex != after.providers.codex || from != to
    }

    /// Whether a save left anything for `catch_up`.
    pub fn is_due(&self) -> bool {
        !lock(&self.due).is_empty()
    }

    /// A refresh's first step, under the refresh lock and before it reads the settings (on a
    /// thread that may wait on folders and rclone): withdraws what saves left behind, then forgets
    /// what the roster sync knew if they asked for that, holding the roster's lock throughout.
    /// Whether the roster sync was reset, so the caller clears what it shows of it.
    pub fn catch_up(&self, files: &mut impl Withdraw) -> bool {
        let mut roster = lock(&self.roster);
        let due = std::mem::take(&mut *lock(&self.due));
        for place in &due.presence {
            files.presence(place);
        }
        for place in &due.roster {
            files.roster(place);
        }
        if due.reset {
            roster.syncer.forget();
            roster.place = None;
        }
        due.reset
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
    pub fn with_syncer<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        f(&mut lock(&self.roster).syncer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

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
        /// Every withdrawal, in order ("presence A", "roster A").
        withdrawals: Vec<String>,
        /// How often the app cleared the roster news it shows (`roster_seen`).
        news_cleared: u32,
    }

    fn name(place: &Place) -> String {
        place.folder.clone().unwrap_or_default()
    }

    struct Io<'a>(&'a Mutex<World>);

    impl Withdraw for Io<'_> {
        fn presence(&mut self, place: &Place) {
            let mut world = self.0.lock().unwrap();
            world.presence.insert(name(place), "withdrawn");
            world.withdrawals.push(format!("presence {}", name(place)));
        }
        fn roster(&mut self, place: &Place) {
            let mut world = self.0.lock().unwrap();
            world.roster.remove(&name(place));
            world.withdrawals.push(format!("roster {}", name(place)));
        }
    }

    /// The app, as far as sharing goes: the live settings, `Sharing`, the files, and a count that
    /// stands for this machine's accounts.
    struct App {
        settings: Mutex<Settings>,
        sharing: Sharing<Syncer>,
        world: Mutex<World>,
        accounts: AtomicU32,
    }

    impl App {
        fn new(settings: Settings) -> Self {
            App { settings: Mutex::new(settings), sharing: Sharing::default(), world: Mutex::default(), accounts: AtomicU32::new(1) }
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
            if self.sharing.is_due() && self.sharing.catch_up(&mut Io(&self.world)) {
                self.world.lock().unwrap().news_cleared += 1;
            }
            self.live()
        }

        /// The rest of a refresh that read `copy`: its presence record, written while `still`
        /// holds, then its roster sync (`app::sync_roster`).
        fn finish(&self, copy: &Place) {
            if still(copy, &self.live()) {
                let live = name(&self.live());
                let mut world = self.world.lock().unwrap();
                world.presence.insert(name(copy), "in use");
                world.writes.push((name(copy), live));
            }
            self.sync(copy);
        }

        /// A roster sync for `copy`: a refresh's, or an agent's (`get_roster` with sync).
        fn sync(&self, copy: &Place) {
            let accounts = self.accounts.load(SeqCst);
            self.sharing.sync_roster(copy, || self.live(), |syncer| {
                if syncer.written != Some(accounts) {
                    syncer.written = Some(accounts);
                    let live = name(&self.live());
                    let mut world = self.world.lock().unwrap();
                    world.roster.insert(name(copy));
                    world.writes.push((name(copy), live));
                }
            });
        }

        fn refresh(&self) {
            let copy = self.start();
            self.finish(&copy);
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
            let world = app.world.lock().unwrap();
            assert_eq!((world.presence["A"], world.roster.clone()), ("in use", BTreeSet::from(["A".to_string()])), "sharing at A");
        }

        let copy = app.start();
        app.accounts.fetch_add(1, SeqCst);
        assert!(app.save(at("B", true)), "a save asks for a refresh");
        assert!(app.save(at("C", true)), "and so does the next");
        app.finish(&copy);
        app.sync(&app.live());
        assert!(app.world.lock().unwrap().roster.contains("C"), "an agent's sync after the move writes at once");
        app.refresh();

        let world = app.world.lock().unwrap();
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
        assert_eq!(app.world.lock().unwrap().roster, BTreeSet::from(["A".to_string(), "C".to_string()]), "C at once; A until the next refresh");
        app.refresh();
        assert_eq!(app.world.lock().unwrap().roster, BTreeSet::from(["C".to_string()]));
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
        assert_eq!(app.world.lock().unwrap().roster, BTreeSet::from(["A".to_string()]), "written again after the withdrawal");

        assert!(app.save(at("A", false)), "turning the roster off asks for a refresh, which withdraws it");
        app.refresh();
        let world = app.world.lock().unwrap();
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
        let world = app.world.lock().unwrap();
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
            let world = app.world.lock().unwrap();
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
        let world = app.world.lock().unwrap();
        assert_eq!((world.presence["B"], world.roster.clone()), ("in use", BTreeSet::from(["B".to_string()])), "sharing at B");
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