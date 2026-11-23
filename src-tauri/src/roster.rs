//! The account roster: which accounts the user has on their other machines, so "Sign in all
//! accounts" on a new machine queues every one of them, with no backup to copy over.
//!
//! Opt-in (`Settings::account_roster`). Each SlopBar writes one file to the in-use sharing
//! location, `<folder>/SlopBar/roster/<machine id>.roster` (or the same path on the rclone
//! remote), listing the accounts signed in on it: provider, account id, email and plan. Never
//! sign-ins, tokens or anything else from the vault.
//!
//! Unlike presence records it names accounts, so it is encrypted as backups are (Argon2id and
//! XChaCha20-Poly1305, see `backup::seal_bytes`) with a roster passphrase the user sets once on
//! each machine and that is kept on it with DPAPI. Nothing the machines already share could be
//! the key instead: the folder is all they have in common, and whoever can read the folder
//! would have the key with it. A roster that does not open with this machine's passphrase is
//! counted (Health says so), never guessed at.
//!
//! Accounts another machine's roster names that are not signed in here are pending (see
//! `bulk::sign_in_plan`): one stops being pending once it is signed in here, and an account
//! removed everywhere else drops out with the last roster naming it.
//!
//! While "Sign in all accounts" runs on a machine, its roster also carries the run's progress
//! (`Run`), rewritten at every step, so the user's other computers can follow it and offer Retry
//! (Settings > Accounts, "Signing in on <machine>"). It names accounts too, so it goes inside the
//! same encrypted file, never beside it in the clear.
//!
//! It also carries each account's latest usage reading taken on that machine (`Reading`), so a
//! machine whose own monitoring sign-in for an account ended, or was never connected, still shows
//! a recent reading of it, marked as the other machine's (`apply_readings`). Claude ends every
//! sign-in about 30 days after it is made, so this lets one machine's sign-ins cover the others.
//! Only readings a machine took itself are written, never one it was given, and only when one
//! changed (coalesced, see `READINGS_GAP_MINUTES`) or to re-date them now and then.
//!
//! Each machine reads its own roster back with the others. When the copy there is not the one it
//! last wrote (a sync service put back an older version, something else wrote over it, or it is
//! gone), it is written again at the next sync (`RESTORE_GAP_MINUTES`), so a restored file cannot
//! keep old readings or an old run in front of the other machines.
//!
//! Over rclone each file read is a process: at the slow pace (`READ_EVERY_MINUTES`) a roster whose
//! name, size and modification time are as the last read found them is not downloaded again
//! (`RosterStore::list_known`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::accounts::Provider;
use crate::backup::{OpenError, Passphrase, SealKey};
use crate::bulk::Pending;
use crate::model::{AccountUsage, SharedReading, UsageWindow};
use crate::rclone::{Known, Listed, RemoteFile};

pub const FORMAT: &str = "slopbar-roster";
const VERSION: u32 = 1;
/// Rosters live here, beside the presence records.
const SUBDIR: [&str; 2] = ["SlopBar", "roster"];
const REMOTE_DIR: &str = "SlopBar/roster";
const EXTENSION: &str = "roster";
/// Rosters are small; anything larger is not one.
const MAX_FILE: u64 = 256 * 1024;
const MAX_ACCOUNTS: usize = 500;
/// Emails, ids and plans longer than this are not kept (they come from other machines).
const FIELD_MAX: usize = 320;
/// A roster not written for this long is from a machine no longer in use and is ignored.
const IGNORE_AFTER_DAYS: i64 = 60;
/// This machine's roster is written when its accounts change, and at least this often.
const REWRITE_AFTER_HOURS: i64 = 24;
/// Other machines' rosters change only when their accounts do: read them at most this often.
const READ_EVERY_MINUTES: i64 = 10;
/// While a run goes on, this machine's roster is rewritten at least this often, so the others can
/// tell a run waiting on the person from a machine that went quiet.
const RUN_HEARTBEAT_MINUTES: i64 = 3;
/// While another machine's run goes on, rosters are read this often; with Settings watching
/// (`soon`), at least this often.
const READ_LIVE_SECONDS: i64 = 15;
const READ_SOON_SECONDS: i64 = 30;
/// After a Retry sent to another machine, its roster is read at the live pace for this long, or
/// until its run changes (see `Syncer::expect_news`).
const EXPECT_MINUTES: i64 = 2;
/// A run's steps beyond this many are not kept (they come from other machines).
const MAX_STEPS: usize = 200;
/// A changed usage reading is written at most this long after the last write: the first refresh
/// after it carries every change since, so an account in use costs a write per gap, not per poll.
const READINGS_GAP_MINUTES: i64 = 15;
/// Readings that have not changed are written again this often, newly dated, so the other
/// machines can tell a reading that still holds from one this machine stopped taking.
const READINGS_HEARTBEAT_HOURS: i64 = 6;
/// The usage windows the providers give (see `providers`); a reading's others are dropped.
const WINDOW_KINDS: [&str; 5] = ["session", "weekly", "weekly_opus", "weekly_sonnet", "window"];
/// Windows beyond this many per reading are not kept (they come from other machines).
const MAX_WINDOWS: usize = 8;
/// A window longer than this is not one the providers give: its length is dropped, and so is a
/// reset further than this from when the reading was taken.
const MAX_WINDOW_SECONDS: i64 = 31 * 86_400;
/// Another machine's reading older than this is not shown: every limit it describes has reset since.
const SHARED_MAX_DAYS: i64 = 7;
/// Another machine's reading of an account covers this machine's monitoring sign-in for it
/// (`AccountUsage::covered_by`) while that machine's own sign-in for it has not lapsed and it is
/// under this old, dated by when it was taken or, if later, by when this machine first saw it or
/// its roster: that machine's rewrites every `READINGS_HEARTBEAT_HOURS` keep it covering, and a
/// reading that stops moving stops covering after this.
pub const COVERED_HOURS: i64 = 12;
/// A roster's plain text is padded to a whole number of these, so the file's size does not follow
/// its readings' digits.
const PAD: usize = 1024;
/// This machine's roster stays under this once sealed, well inside `MAX_FILE`, which every
/// SlopBar's reader (older ones too) holds rosters to: its oldest readings go first.
const WRITE_LIMIT: u64 = 192 * 1024;
/// Another machine's roster missing from one read (a sync app between versions, a download that
/// failed) is kept from the read before for this long, so its readings do not blink out.
const KEEP_MISSING_MINUTES: i64 = 2 * READ_EVERY_MINUTES;
/// This machine's roster found not as it last wrote it is written again at the next sync. Found so
/// again and again (something else writes the same file: a copy of this machine with the same id,
/// or a sync service that keeps putting an older copy back), it is written again only after these
/// waits since the last time, one more each time and the last one over and over, so a fight costs
/// a write every few hours, not one per read; a day without one starts them over.
const RESTORE_WAITS_MINUTES: [i64; 3] = [15, 60, 6 * 60];
const RESTORE_CALM_HOURS: i64 = 24;
const PASSPHRASE_FILE: &str = "roster-passphrase.bin";
const MAX_PASSPHRASE: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub provider: Provider,
    pub id: String,
    pub email: Option<String>,
    pub plan: Option<String>,
}

/// What one roster file holds once opened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roster {
    pub v: u32,
    pub machine_id: String,
    pub machine: String,
    pub written_at: DateTime<Utc>,
    pub accounts: Vec<Entry>,
    /// "Sign in all accounts" as it runs there, or last ran since SlopBar started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    /// Each account's latest usage reading taken there. A roster from a SlopBar without readings
    /// opens with none, and one without them ignores these. Read one by one: one this SlopBar
    /// cannot read (a provider it does not know, say) is dropped, not the roster.
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "each_reading")]
    pub readings: Vec<Reading>,
}

fn each_reading<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Reading>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    Ok(raw.as_array().into_iter().flatten().take(2 * MAX_ACCOUNTS).filter_map(|v| Reading::deserialize(v).ok()).collect())
}

/// One account's usage as the machine that wrote the roster last read it from the provider
/// itself (`AccountUsage::windows` and `fetched_at` there): never a reading it was given, so none
/// goes round. Only the windows: no credits, resets held, billing, errors or sign-in data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub provider: Provider,
    pub id: String,
    /// When that machine read it, by its clock.
    pub taken_at: DateTime<Utc>,
    pub windows: Vec<UsageWindow>,
    /// That machine's sign-in for it has lapsed: its latest read failed for want of a new sign-in
    /// (`monitoring::SIGN_IN_ERRORS`), so this is the last reading it takes until someone signs in
    /// there again: shown, but it covers nothing (`covered_by`). A read that failed for a moment (the
    /// network, a rate limit) lapses nothing; one that keeps failing stops covering after
    /// `COVERED_HOURS` anyway. Absent from older rosters: false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lapsed: bool,
}

/// "Sign in all accounts" on the machine that wrote the roster, as Settings > Accounts shows it
/// there (see `app::BulkConnect`), for the user's other computers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// Bumped at every change there; a lower one is older news.
    pub revision: u64,
    /// When it last changed, by that machine's clock.
    pub at: DateTime<Utc>,
    pub total: usize,
    pub done: usize,
    /// How many emails, and which one (1-based) is signing in now.
    pub emails: usize,
    pub email: usize,
    pub finished: bool,
    /// The open sign-in has waited long enough that the person there was asked whether to retry.
    pub still_waiting: bool,
    pub monitoring: bool,
    pub error: Option<String>,
    pub steps: Vec<RunStep>,
    /// "Sign in all your computers from here" (see `relay`): its pages open at that machine, and
    /// Retry from another computer never restarts it.
    #[serde(default)]
    pub everywhere: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStep {
    pub provider: Provider,
    /// Alias, else email: whom to sign in as.
    pub account: Option<String>,
    /// add, renew or connect (`bulk::Kind`). An older SlopBar drops a kind it does not know.
    pub kind: String,
    /// pending, running, done, skipped or failed.
    pub state: String,
    pub error: Option<String>,
}

/// This machine's roster: every account signed in here, field by field so nothing else from an
/// account can reach the file.
pub fn own(machine_id: &str, machine: &str, accounts: &[AccountUsage], now: DateTime<Utc>) -> Roster {
    let mut list: Vec<Entry> = accounts.iter()
        .map(|a| Entry { provider: a.provider, id: a.id.clone(), email: a.email.clone(), plan: a.plan.clone() })
        .collect();
    list.sort_by(|a, b| (a.provider.key(), &a.id).cmp(&(b.provider.key(), &b.id)));
    list.dedup_by(|a, b| a.provider == b.provider && a.id == b.id);
    Roster { v: VERSION, machine_id: machine_id.into(), machine: machine.into(), written_at: now, accounts: list, run: None, readings: vec![] }
}

/// This machine's own readings: each account's last successful read here, from its `windows` and
/// `fetched_at`, never `shared`, which another machine took. Not one older than `SHARED_MAX_DAYS`,
/// which no other machine would show, nor of an account known only by its token (`token:` ids
/// hash each machine's own sign-in, so no other machine has the same one).
pub fn readings(accounts: &[AccountUsage], now: DateTime<Utc>) -> Vec<Reading> {
    let mut list: Vec<Reading> = accounts.iter()
        .filter(|a| !a.windows.is_empty() && !a.id.starts_with("token:"))
        .filter_map(|a| Some(Reading { provider: a.provider, id: a.id.clone(), taken_at: a.fetched_at?, windows: a.windows.clone(), lapsed: lapsed(a) }))
        .filter(|r| now - r.taken_at <= Duration::days(SHARED_MAX_DAYS))
        .collect();
    list.sort_by(|a, b| (a.provider.key(), &a.id).cmp(&(b.provider.key(), &b.id)));
    list.dedup_by(|a, b| a.provider == b.provider && a.id == b.id);
    list
}

/// This machine's sign-in for an account has lapsed (`Reading::lapsed`): its latest read failed for
/// want of a new sign-in, as `monitoring::renewal` reads the same errors.
fn lapsed(a: &AccountUsage) -> bool {
    a.stale && a.error.as_deref().is_some_and(|e| crate::monitoring::SIGN_IN_ERRORS.contains(&e))
}

/// Whether two machines' worth of readings show the same: the same accounts and windows, each
/// window's percent the same once rounded, as the flyout shows it, and its reset within a minute,
/// and each sign-in lapsed or not alike. When they were taken does not count: re-dating alone is the
/// heartbeat's job. Nor does the reset of a window not used since it last reset (`unused_alike`).
fn same_readings(a: &[Reading], b: &[Reading]) -> bool {
    let near = |x: Option<DateTime<Utc>>, y: Option<DateTime<Utc>>| match (x, y) {
        (Some(x), Some(y)) => (x - y).num_seconds().abs() < 60,
        (x, y) => x.is_none() && y.is_none(),
    };
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| {
        x.provider == y.provider && x.id == y.id && x.lapsed == y.lapsed && x.windows.len() == y.windows.len()
            && x.windows.iter().zip(&y.windows).all(|(v, w)| v.kind == w.kind && v.window_seconds == w.window_seconds
                && v.used_percent.round() == w.used_percent.round()
                && (near(v.resets_at, w.resets_at) || unused_alike(v, x.taken_at, w, y.taken_at)))
    })
}

/// Two readings of a window not used since it last reset. Codex gives such a window's reset as one
/// window length after the read (`notStarted` in i18n.js), a time that moves with every read: they
/// are alike while each reset lies as far from its own read, within 15 minutes, which holds
/// whatever the difference between this machine's clock and the provider's.
fn unused_alike(v: &UsageWindow, v_taken: DateTime<Utc>, w: &UsageWindow, w_taken: DateTime<Utc>) -> bool {
    v.used_percent <= 0.0 && w.used_percent <= 0.0 && match (v.resets_at, w.resets_at) {
        (Some(x), Some(y)) => ((x - v_taken) - (y - w_taken)).num_seconds().abs() <= 15 * 60,
        _ => false,
    }
}

/// The file name for a machine id: its hex digits and dashes only.
pub fn file_name(machine_id: &str) -> Option<String> {
    let safe: String = machine_id.chars().filter(|c| c.is_ascii_hexdigit() || *c == '-').collect();
    (!safe.is_empty()).then(|| format!("{safe}.{EXTENSION}"))
}

/// Seals a roster, its plain text padded to a whole number of `PAD` bytes. One that would pass
/// `WRITE_LIMIT` (hundreds of accounts) goes without its oldest readings: a roster over `MAX_FILE`
/// is skipped by every reader, accounts and all.
pub fn seal(roster: &Roster, key: &SealKey) -> Result<Vec<u8>, String> {
    let padded = |r: &Roster| -> Result<Vec<u8>, String> {
        let mut plain = serde_json::to_vec(r).map_err(|_| "roster_unwritable")?;
        // `{…}` becomes `{…,"pad":"   "}`; readers ignore the field.
        if plain.pop() != Some(b'}') {
            return Err("roster_unwritable".into());
        }
        let field: &[u8] = b",\"pad\":\"";
        let spaces = (PAD - (plain.len() + field.len() + 2) % PAD) % PAD;
        plain.extend_from_slice(field);
        plain.resize(plain.len() + spaces, b' ');
        plain.extend_from_slice(b"\"}");
        crate::backup::seal_bytes(FORMAT, &plain, key)
    };
    let sealed = padded(roster)?;
    if sealed.len() as u64 <= WRITE_LIMIT || roster.readings.is_empty() {
        return Ok(sealed);
    }
    let mut newest = roster.readings.clone();
    newest.sort_by(|a, b| b.taken_at.cmp(&a.taken_at));
    loop {
        newest.truncate(newest.len() * 3 / 4);
        let mut readings = newest.clone();
        readings.sort_by(|a, b| (a.provider.key(), &a.id).cmp(&(b.provider.key(), &b.id)));
        let sealed = padded(&Roster { readings, ..roster.clone() })?;
        if sealed.len() as u64 <= WRITE_LIMIT || newest.is_empty() {
            return Ok(sealed);
        }
    }
}

/// Opens a roster. `keys` keeps every key derived so far by salt, so each machine's roster costs
/// one Argon2id derivation per run, not one per read; it must be emptied when the passphrase
/// changes.
pub fn open(bytes: &[u8], passphrase: &Passphrase, keys: &mut HashMap<String, [u8; 32]>) -> Result<Roster, OpenError> {
    let plain = crate::backup::open_bytes(FORMAT, bytes, MAX_FILE, &mut |request| {
        let id = request.id();
        if let Some(key) = keys.get(&id) {
            return Ok(*key);
        }
        let key = request.derive(passphrase)?;
        keys.insert(id, key);
        Ok(key)
    })?;
    let mut roster: Roster = serde_json::from_slice(&plain).map_err(|_| OpenError::Unsupported)?;
    if roster.v != VERSION {
        return Err(OpenError::Unsupported);
    }
    let fits = |s: &String| s.len() <= FIELD_MAX && !s.chars().any(char::is_control);
    roster.accounts.retain(|e| fits(&e.id) && !e.id.is_empty() && e.email.as_ref().is_none_or(fits) && e.plan.as_ref().is_none_or(fits));
    roster.accounts.truncate(MAX_ACCOUNTS);
    roster.machine = roster.machine.chars().filter(|c| !c.is_control() && !invisible(*c)).take(63).collect::<String>().trim().into();
    if let Some(run) = roster.run.as_mut() {
        let one_of = |s: &String, words: &[&str]| words.contains(&s.as_str());
        let code = |s: &String| s.len() <= 200 && !s.chars().any(char::is_control);
        run.steps.retain(|s| one_of(&s.kind, &["add", "renew", "connect"]) && one_of(&s.state, &["pending", "running", "done", "skipped", "failed"])
            && s.account.as_ref().is_none_or(fits) && s.error.as_ref().is_none_or(code));
        run.steps.truncate(MAX_STEPS);
        if run.error.as_ref().is_some_and(|e| !code(e)) {
            run.error = Some("unknown".into());
        }
    }
    // Readings: only of accounts the roster lists, one each, with windows the providers give.
    for r in roster.readings.iter_mut() {
        r.windows.retain(|w| WINDOW_KINDS.contains(&w.kind.as_str()) && w.used_percent.is_finite() && (0.0..=1000.0).contains(&w.used_percent));
        r.windows.truncate(MAX_WINDOWS);
        let taken = r.taken_at;
        for w in r.windows.iter_mut() {
            if w.window_seconds.is_some_and(|s| !(1..=MAX_WINDOW_SECONDS).contains(&s)) {
                w.window_seconds = None;
            }
            if w.resets_at.is_some_and(|at| (at - taken).num_seconds().abs() > MAX_WINDOW_SECONDS) {
                w.resets_at = None;
            }
        }
    }
    let accounts = &roster.accounts;
    roster.readings.retain(|r| !r.windows.is_empty() && accounts.iter().any(|e| e.provider == r.provider && e.id == r.id));
    roster.readings.sort_by(|a, b| (a.provider.key(), &a.id, b.taken_at).cmp(&(b.provider.key(), &b.id, a.taken_at)));
    roster.readings.dedup_by(|a, b| a.provider == b.provider && a.id == b.id);
    roster.readings.truncate(MAX_ACCOUNTS);
    Ok(roster)
}

/// Characters that change how a name shows without showing themselves (Unicode's default-ignorable
/// code points: zero-width, direction marks, variation selectors, fillers, tags and their kin, and
/// the interlinear annotation marks): a name from another machine loses them, and its spaces at
/// either end, so "DESK-1" with one added cannot pass for this PC's "DESK-1". Letters of another
/// script that look alike are not caught this way; the reading is marked as another machine's.
/// The line and paragraph separators go too: a name is one line wherever it is shown or quoted.
pub(crate) fn invisible(c: char) -> bool {
    matches!(c, '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}' | '\u{180B}'..='\u{180F}'
        | '\u{200B}'..='\u{200F}' | '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}'
        | '\u{FFA0}' | '\u{FFF0}'..='\u{FFFB}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E0FFF}')
}

// ---------------------------------------------------------------------------------------------
// Storage: the same place presence records go (see `presence::store`).

pub trait RosterStore: Send + Sync {
    /// Every roster file there: its name, bytes and modification time (when known).
    #[cfg(test)]
    fn list(&self) -> std::io::Result<Vec<(String, Vec<u8>, Option<DateTime<Utc>>)>> {
        Ok(self.listing(&|_, _, _| None)?.files.into_iter().map(|f| (f.name, f.bytes, f.modified)).collect())
    }
    /// Every roster file there (its name, bytes and modification time, when known), with the names
    /// listed whose bytes could not be read (neither gone nor changed), and each file's content
    /// hash where the storage lists one at no cost: an rclone remote on a cloud whose listing carries
    /// them (`Rclone::files_listed`). A file listed with a hash goes to `known` first, and the bytes
    /// it gives back, kept from a read of the file with that hash, stand in for downloading it again
    /// (a process each over rclone).
    fn listing(&self, known: Known) -> std::io::Result<Listed>;
    /// Replaces the file `name` in one step where the storage can: a folder renames a finished
    /// temporary file over it, and an rclone remote uploads straight over it (`Rclone::put`).
    fn put(&self, name: &str, bytes: &[u8]) -> std::io::Result<()>;
    /// Deletes the file `name`; one already gone is fine.
    fn remove(&self, name: &str) -> std::io::Result<()>;
}

/// Where the settings share in-use status, whether or not the roster is on (turning it off
/// removes this machine's roster from there).
pub fn store(settings: &crate::settings::Settings) -> Option<Box<dyn RosterStore>> {
    if let Some(remote) = &settings.presence_remote {
        return crate::rclone::Rclone::open(remote).map(|s| Box::new(s) as Box<dyn RosterStore>);
    }
    settings.presence_folder.as_ref().map(|f| Box::new(Folder(PathBuf::from(f))) as Box<dyn RosterStore>)
}

/// The synced folder the user chose for in-use sharing.
pub struct Folder(pub PathBuf);

impl Folder {
    fn dir(&self) -> PathBuf {
        SUBDIR.iter().fold(self.0.clone(), |p, s| p.join(s))
    }
}

impl RosterStore for Folder {
    /// Every file is read: a folder lists no hashes, and a read costs no process.
    fn listing(&self, _: Known) -> std::io::Result<Listed> {
        let entries = match std::fs::read_dir(self.dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Listed::default()),
            Err(e) => return Err(e),
        };
        let mut listed = Listed::default();
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some(EXTENSION) {
                continue;
            }
            let Some(meta) = e.metadata().ok().filter(|m| m.is_file() && m.len() <= MAX_FILE) else { continue };
            let name = e.file_name().to_string_lossy().into_owned();
            match std::fs::read(&path) {
                Ok(bytes) => listed.files.push(RemoteFile { name, bytes, modified: meta.modified().ok().map(DateTime::<Utc>::from), hash: None }),
                // A file the sync app holds, or one only in the cloud that could not be fetched.
                Err(_) => listed.unread.push(name),
            }
        }
        Ok(listed)
    }

    fn put(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        if !self.0.is_dir() {
            return Err(std::io::Error::other("sharing folder missing"));
        }
        crate::private_store::write(&self.dir().join(name), bytes)
    }

    fn remove(&self, name: &str) -> std::io::Result<()> {
        match std::fs::remove_file(self.dir().join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

impl<R: crate::rclone::Runner> RosterStore for crate::rclone::Rclone<R> {
    fn listing(&self, known: Known) -> std::io::Result<Listed> {
        self.files_listed(REMOTE_DIR, EXTENSION, MAX_FILE, true, known)
    }

    fn put(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        crate::rclone::Rclone::put(self, REMOTE_DIR, name, bytes)
    }

    fn remove(&self, name: &str) -> std::io::Result<()> {
        self.delete(REMOTE_DIR, name)
    }
}

/// Removes this machine's roster: the roster was turned off, or sharing moved elsewhere.
pub fn withdraw(store: &dyn RosterStore) -> std::io::Result<()> {
    match crate::presence::identity().and_then(|(id, _)| file_name(id)) {
        Some(name) => store.remove(&name),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// The passphrase, kept on this machine with DPAPI.

pub fn passphrase_path(dir: &Path) -> PathBuf { dir.join(PASSPHRASE_FILE) }

pub fn load_passphrase(dir: &Path) -> Option<Passphrase> {
    let bytes = crate::private_store::load_secret(&passphrase_path(dir)).ok()?;
    String::from_utf8(bytes).ok().map(Passphrase).filter(valid_passphrase)
}

fn valid_passphrase(p: &Passphrase) -> bool {
    (crate::backup::MIN_PASSPHRASE..=MAX_PASSPHRASE).contains(&p.0.chars().count())
}

/// Saves the passphrase with DPAPI, or deletes it for None.
pub fn store_passphrase(dir: &Path, passphrase: Option<&Passphrase>) -> Result<(), String> {
    match passphrase {
        None => match std::fs::remove_file(passphrase_path(dir)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err("roster_passphrase_storage".into()),
            _ => Ok(()),
        },
        Some(p) if !valid_passphrase(p) => Err("roster_passphrase_too_short".into()),
        Some(p) => crate::private_store::save_secret(&passphrase_path(dir), p.0.as_bytes()).map_err(|_| "roster_passphrase_storage".into()),
    }
}

// ---------------------------------------------------------------------------------------------
// Writing and reading, once per refresh

/// Another machine's roster.
#[derive(Debug, Clone, Serialize)]
pub struct Peer {
    pub machine_id: String,
    pub machine: String,
    pub written_at: DateTime<Utc>,
    pub accounts: Vec<Entry>,
    pub run: Option<Run>,
    /// Its readings, none later than `written_at`.
    #[serde(skip)]
    pub readings: Vec<Received>,
    /// When a read last found its roster, by this machine's clock (`KEEP_MISSING_MINUTES`).
    #[serde(skip)]
    pub listed_at: DateTime<Utc>,
}

/// Another machine's reading as this one received it.
#[derive(Debug, Clone, PartialEq)]
pub struct Received {
    pub reading: Reading,
    /// When this machine first saw it taken at that time, by its own clock: a reading that stops
    /// moving forward stops covering an account however it is dated (`COVERED_HOURS`).
    pub seen_at: DateTime<Utc>,
}

impl Peer {
    /// Its run is still going, by its last word.
    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.finished)
    }
}

/// What the last write and read found, for Settings, Health and the door.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Seen {
    /// Whether the last write of this machine's roster worked, and when one last did.
    pub wrote: Option<bool>,
    pub written_at: Option<DateTime<Utc>>,
    /// Why the last write failed, when the network's proxy stopped it (`proxy::refusal_code`).
    pub write_error: Option<&'static str>,
    /// Whether the last read worked, and when it was.
    pub read: Option<bool>,
    pub read_at: Option<DateTime<Utc>>,
    /// Why the last read failed, when the network's proxy stopped it.
    pub read_error: Option<&'static str>,
    /// This machine's roster as read back from the sharing location: how many accounts it names.
    pub own_accounts: Option<usize>,
    /// Newest first.
    pub peers: Vec<Peer>,
    /// Rosters that did not open with this machine's passphrase (or were changed).
    pub unreadable: usize,
    /// Rosters from a newer SlopBar.
    pub unsupported: usize,
    /// How many times this machine's roster was found not as it last wrote it, or gone, and was
    /// written again (`Syncer::restore`), since the roster sync last started afresh (SlopBar started,
    /// or the passphrase or sharing location changed). One that keeps rising means something else
    /// writes this machine's file: Health says so from `OWN_REWRITTEN_WARN`.
    pub own_rewritten: usize,
}

impl Seen {
    /// No sync could run (no sharing location, rclone or passphrase here, no machine identity, or
    /// sharing moved meanwhile): the last write's and read's proxy reasons no longer say why, so
    /// they go. The rest stays as it was.
    pub fn unsynced(&mut self) {
        self.write_error = None;
        self.read_error = None;
    }
}

/// Kept between refreshes: derived keys, and what was written last.
#[derive(Default)]
pub struct Syncer {
    keys: HashMap<String, [u8; 32]>,
    seal: Option<SealKey>,
    /// The accounts and readings last written, and when.
    written: Option<(Vec<Entry>, Vec<Reading>, DateTime<Utc>)>,
    /// This machine's run, newest revision (see `offer_run`), and the one last written.
    run: Option<Run>,
    written_run: Option<Run>,
    /// `run` was taken back from this machine's own roster after a restart; any new run replaces
    /// it, whatever its revision (revisions start again with the app).
    adopted: bool,
    /// Whether the first sync since the app started has run.
    started: bool,
    /// Machines sent a Retry: until when their news is expected, and their run's time then.
    expecting: HashMap<String, (DateTime<Utc>, Option<DateTime<Utc>>)>,
    /// When each of the other machines' readings was first seen taken at its time (`Received`).
    first_seen: HashMap<(String, Provider, String), (DateTime<Utc>, DateTime<Utc>)>,
    /// When each version of the other machines' rosters (machine id, as dated) was first seen.
    rosters_seen: HashMap<(String, DateTime<Utc>), DateTime<Utc>>,
    /// When a write for readings alone, or to put this machine's roster back (`restore`), was last
    /// tried, whether or not it worked: the next waits out the gap either way, so a sharing
    /// location that is down costs one try per gap.
    readings_tried: Option<DateTime<Utc>>,
    /// Settings: "Share usage readings" is off, so this machine's roster carries none (and is
    /// rewritten without them at once).
    pub withhold_readings: bool,
    /// Whether the last sync withheld readings: the first sync that does tries at once.
    withheld: bool,
    /// The rosters the last read used (this machine's and the other machines' own files) that were
    /// listed with a content hash: by name, that hash and the bytes read. At the slow pace a file
    /// listed again with the same hash and size is not downloaded again (`RosterStore::listing`).
    files: HashMap<String, (String, Vec<u8>)>,
    /// The SHA-256 of the sealed roster this run last wrote: what its own file must read back as.
    written_hash: Option<[u8; 32]>,
    /// The rosters whose writes failed since then: a write cut short by its time limit may have
    /// landed all the same, so finding one of them there is no reason to write again.
    tried_hashes: Vec<[u8; 32]>,
    /// The last read found this machine's roster not as it last wrote it, or not at all: it is
    /// written again at the next sync (`RESTORE_WAITS_MINUTES`).
    restore: bool,
    /// When a write for that reason was last tried, whether or not it worked, and how many were
    /// tried with less than `RESTORE_CALM_HOURS` between one and the next.
    restore_tried: Option<DateTime<Utc>>,
    restores: usize,
    pub seen: Seen,
}

/// What a sealed roster must read back as (`Syncer::written_hash`).
fn digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

/// Failed writes whose rosters are still accepted as this machine's (`Syncer::tried_hashes`).
const TRIED_KEPT: usize = 4;

/// This machine's own file as a read found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Own {
    /// Not listed.
    Absent,
    /// Listed, but its bytes could not be read: neither gone nor changed.
    Unread,
    /// A roster this run wrote, or tried to.
    Written,
    /// Anything else: an older copy put back, another file written over it, damage.
    Other,
}

impl Syncer {
    /// Forgets everything that depends on the passphrase or the location: after either changes,
    /// or the roster is turned off.
    pub fn reset(&mut self) {
        *self = Syncer::default();
    }

    /// Takes this machine's run as it is now, unless a newer revision is already in hand (syncs
    /// started one after another can finish in any order).
    pub fn offer_run(&mut self, run: Run) {
        if self.adopted || self.run.as_ref().is_none_or(|r| r.revision < run.revision) {
            self.run = Some(run);
            self.adopted = false;
        }
    }

    /// A Retry was sent to `machine_id`: its roster is read at the live pace until its run changes
    /// or `EXPECT_MINUTES` pass, so the new run shows within seconds of its upload.
    pub fn expect_news(&mut self, machine_id: &str, now: DateTime<Utc>) {
        let at = self.seen.peers.iter().find(|p| p.machine_id == machine_id).and_then(|p| p.run.as_ref()).map(|r| r.at);
        self.expecting.insert(machine_id.into(), (now + Duration::minutes(EXPECT_MINUTES), at));
    }

    /// One refresh's turn: writes this machine's roster when its accounts or its run changed (or a
    /// day passed, or a few minutes while its run goes on), or a reading changed and the last write
    /// is `READINGS_GAP_MINUTES` old (or `READINGS_HEARTBEAT_HOURS`, to re-date readings taken
    /// since), or the last read found it not as written (`restore`), then reads every roster when
    /// due, or at once with `force`. With `soon`, Settings is watching, so rosters are read more
    /// often.
    #[allow(clippy::too_many_arguments)]
    pub fn sync(&mut self, store: &dyn RosterStore, passphrase: &Passphrase, me: (&str, &str), accounts: &[AccountUsage], now: DateTime<Utc>, force: bool, soon: bool) {
        let Some(name) = file_name(me.0) else { return };
        // After a restart, this machine's last failed run is taken back from its own roster before
        // the first write would replace it, so the other computers keep its Retry for the hour.
        let read_first = !self.started && self.run.is_none();
        self.started = true;
        if read_first {
            // Not one dated ahead of this clock (as another machine's roster is not trusted then):
            // it would count as recent for as long as its date lies ahead, and be written on.
            let recent = |r: &Run| r.finished && r.error.is_some() && r.at <= now + Duration::minutes(5) && now - r.at.min(now) < Duration::minutes(SHOW_FINISHED_MINUTES);
            if let Some(run) = self.read_all(store, passphrase, me.0, &name, now, false).filter(recent) {
                self.written_run = Some(run.clone());
                self.run = Some(run);
                self.adopted = true;
            }
        }
        let mut mine = own(me.0, me.1, accounts, now);
        mine.run = self.run.clone();
        if self.withhold_readings && !self.withheld {
            self.readings_tried = None;
        }
        self.withheld = self.withhold_readings;
        if !self.withhold_readings {
            mine.readings = readings(accounts, now);
        }
        let accounts_due = force || self.written.as_ref().is_none_or(|(list, _, at)| *list != mine.accounts || now - *at >= Duration::hours(REWRITE_AFTER_HOURS));
        let run_due = mine.run != self.written_run
            || mine.run.as_ref().is_some_and(|r| !r.finished) && self.seen.written_at.is_none_or(|at| now - at >= Duration::minutes(RUN_HEARTBEAT_MINUTES));
        // Every write carries the readings as they are. Readings that changed wait out the gap since
        // the last write or try; readings only taken since (unchanged numbers, newer times) wait for
        // the heartbeat; readings no longer shared go at once.
        let readings_due = self.written.as_ref().is_some_and(|(_, written, at)| {
            let waited = |gap: Duration| now - (*at).max(self.readings_tried.unwrap_or(*at)) >= gap;
            let newer = mine.readings.iter().any(|r| written.iter().find(|w| w.provider == r.provider && w.id == r.id).is_none_or(|w| r.taken_at > w.taken_at));
            self.withhold_readings && !written.is_empty() && self.readings_tried.is_none_or(|t| t <= *at || now - t >= Duration::minutes(READINGS_GAP_MINUTES))
                || !same_readings(written, &mine.readings) && waited(Duration::minutes(READINGS_GAP_MINUTES))
                || newer && waited(Duration::hours(READINGS_HEARTBEAT_HOURS))
        });
        // The last read found this machine's roster not as it was written, or gone (a sync service
        // put back an older copy, say): written again now, and while it keeps being found so, after
        // each of `RESTORE_WAITS_MINUTES`. Like a write of readings alone, it waits out the gap after
        // one of them was tried and failed, so a sharing location that is down costs a try a gap.
        let failed_lately = self.readings_tried.is_some_and(|t| self.written.as_ref().is_none_or(|(_, _, at)| t > *at) && now - t < Duration::minutes(READINGS_GAP_MINUTES));
        let restores = match self.restore_tried {
            Some(t) if now - t < Duration::hours(RESTORE_CALM_HOURS) => self.restores,
            _ => 0,
        };
        let waited = restores == 0 || self.restore_tried.is_some_and(|t| now - t >= Duration::minutes(RESTORE_WAITS_MINUTES[restores.min(RESTORE_WAITS_MINUTES.len()) - 1]));
        let restore_due = self.restore && !failed_lately && waited;
        let mut wrote = false;
        if accounts_due || run_due || readings_due || restore_due {
            if self.seal.is_none() {
                self.seal = SealKey::new(passphrase).ok();
            }
            if (readings_due || restore_due) && !(accounts_due || run_due) {
                self.readings_tried = Some(now);
            }
            if restore_due {
                self.restore_tried = Some(now);
                self.restores = restores + 1;
            }
            let sealed = self.seal.as_ref().and_then(|key| seal(&mine, key).ok());
            let put = sealed.as_ref().map(|bytes| store.put(&name, bytes));
            let ok = matches!(put, Some(Ok(())));
            self.seen.wrote = Some(ok);
            self.seen.write_error = put.and_then(Result::err).as_ref().and_then(crate::proxy::refusal_code);
            let hash = sealed.as_deref().map(digest);
            if ok {
                self.written = Some((mine.accounts.clone(), mine.readings.clone(), now));
                self.written_run = mine.run.clone();
                self.seen.written_at = Some(now);
                self.written_hash = hash;
                self.tried_hashes.clear();
                // Whatever made it due, the file is the one written now.
                if std::mem::take(&mut self.restore) {
                    self.seen.own_rewritten += 1;
                }
                // A change of accounts is worth reading back; a step of a run is not.
                wrote = accounts_due;
            } else if let Some(hash) = hash {
                // It may have landed all the same (a write cut off at its time limit).
                self.tried_hashes.push(hash);
                if self.tried_hashes.len() > TRIED_KEPT {
                    self.tried_hashes.remove(0);
                }
            }
        }
        // A machine sent a Retry stays live until its run changes or the wait runs out.
        let peers = &self.seen.peers;
        self.expecting.retain(|id, (until, at)| {
            now < *until && peers.iter().find(|p| &p.machine_id == id).and_then(|p| p.run.as_ref()).map(|r| r.at) == *at
        });
        // A run whose machine went quiet (`QUIET_MINUTES`) keeps no pace: one that stopped for
        // good would hold every other machine at the live pace until its roster is ignored.
        let live = self.seen.peers.iter().any(|p| p.running() && now - p.written_at <= Duration::minutes(QUIET_MINUTES));
        let every = if live || !self.expecting.is_empty() {
            Duration::seconds(READ_LIVE_SECONDS)
        } else if soon {
            Duration::seconds(READ_SOON_SECONDS)
        } else {
            Duration::minutes(READ_EVERY_MINUTES)
        };
        let read_due = self.seen.read_at.is_none_or(|t| now - t >= every);
        // Just read before the write: not again (what was written is what a read-back would find).
        if read_first {
            if wrote {
                self.seen.own_accounts = self.written.as_ref().map(|(list, _, _)| list.len());
            }
            return;
        }
        if !(force || wrote || read_due) {
            return;
        }
        // A file listed with the content hash it had when it was last downloaded is not downloaded
        // again, at any pace: its bytes are those. A forced read (sync_roster) downloads everything.
        self.read_all(store, passphrase, me.0, &name, now, !force);
    }

    /// Reads every roster; returns the run this machine's own roster carries. With `known`, a file
    /// listed with the content hash (and size) of the bytes the last read kept of it is not read
    /// again (`RosterStore::listing`): those bytes stand in for it.
    fn read_all(&mut self, store: &dyn RosterStore, passphrase: &Passphrase, own_id: &str, own_name: &str, now: DateTime<Utc>, known: bool) -> Option<Run> {
        self.seen.read_at = Some(now);
        let listed = if known {
            let kept = &self.files;
            store.listing(&|name: &str, size: u64, hash: &str| {
                kept.get(name).filter(|(h, bytes)| h == hash && bytes.len() as u64 == size).map(|(_, bytes)| bytes.clone())
            })
        } else {
            store.listing(&|_, _, _| None)
        };
        match listed {
            Err(e) => {
                self.seen.read = Some(false);
                self.seen.read_error = crate::proxy::refusal_code(&e);
                None
            }
            Ok(listed) => {
                self.seen.read = Some(true);
                self.seen.read_error = None;
                let mut hashed: HashMap<String, (String, Vec<u8>)> = listed.files.iter()
                    .filter_map(|f| Some((f.name.clone(), (f.hash.clone()?, f.bytes.clone()))))
                    .collect();
                let (run, used) = self.read(listed, passphrase, own_id, own_name, now);
                // Kept: the rosters this read used, so files nobody reads cost nothing to keep.
                hashed.retain(|name, _| used.contains(name));
                self.files = hashed;
                run
            }
        }
    }

    /// Reads the files a listing found: returns the run this machine's own roster carries, and the
    /// names of the files used (this machine's own and the other machines' own files).
    fn read(&mut self, listed: Listed, passphrase: &Passphrase, own_id: &str, own_name: &str, now: DateTime<Utc>) -> (Option<Run>, Vec<String>) {
        let (mut unreadable, mut unsupported, mut own_accounts, mut own_run) = (0, 0, None, None);
        let mut own = if listed.unread.iter().any(|n| n == own_name) { Own::Unread } else { Own::Absent };
        let mut used = Vec::new();
        let mut peers: HashMap<String, Peer> = HashMap::new();
        let mut rosters_seen = HashMap::new();
        for RemoteFile { name, bytes, modified, .. } in listed.files {
            // This machine's own file: a roster this run wrote (or tried to), byte for byte, or not
            // (an older copy put back, another file written over it, damage).
            let mine = name == own_name;
            if mine {
                let hash = digest(&bytes);
                own = if self.written_hash == Some(hash) || self.tried_hashes.contains(&hash) { Own::Written } else { Own::Other };
            }
            let roster = match open(&bytes, passphrase, &mut self.keys) {
                Ok(r) => r,
                Err(OpenError::WrongOrDamaged) => { unreadable += 1; continue; }
                Err(_) => { unsupported += 1; continue; }
            };
            // A clock running ahead is not trusted: the storage's time is used instead.
            let ahead = roster.written_at > now + Duration::minutes(5);
            let written_at = if ahead { modified.unwrap_or(now).min(now) } else { roster.written_at };
            // This machine's roster is the file named after it, saying it is this machine's, as
            // another machine's is; one under its name that says it is another's is nobody's.
            if mine {
                if roster.machine_id == own_id {
                    own_accounts = Some(roster.accounts.len());
                    own_run = roster.run;
                    used.push(name);
                } else {
                    own = Own::Other;
                }
                continue;
            }
            // Each machine writes under its own id's name (`file_name`): another file claiming it,
            // a sync app's conflicted copy say, does not stand in for it, this machine included.
            if file_name(&roster.machine_id).as_deref() != Some(name.as_str()) {
                continue;
            }
            used.push(name);
            // When this machine first saw this version of the roster, dated ahead or not: its
            // readings are dated from then at the latest, so one left dated ahead does not wake up
            // fresh once this clock reaches its date. Remembered for this run of SlopBar only: after
            // a restart such a roster counts as first seen then.
            let version = (roster.machine_id.clone(), roster.written_at);
            let first = self.rosters_seen.get(&version).copied().unwrap_or(now);
            rosters_seen.insert(version, first);
            if now - written_at > Duration::days(IGNORE_AFTER_DAYS) {
                continue;
            }
            // A reading cannot have been taken after its roster was written. A roster dated ahead
            // of this clock gets the storage's time, which would date its readings now at every
            // read: they are not taken at all.
            // Nor are the readings of a machine whose name was all invisible: none could say whose.
            let readings = if ahead || roster.machine.is_empty() { vec![] } else { roster.readings };
            let readings = readings.into_iter().map(|mut reading| {
                reading.taken_at = reading.taken_at.min(written_at);
                // A reset that far from the reading, once it is dated, is not one the providers give.
                let taken = reading.taken_at;
                for w in reading.windows.iter_mut().filter(|w| w.resets_at.is_some_and(|at| (at - taken).num_seconds().abs() > MAX_WINDOW_SECONDS)) {
                    w.resets_at = None;
                }
                let seen_at = match self.first_seen.get(&(roster.machine_id.clone(), reading.provider, reading.id.clone())) {
                    Some((taken, seen)) if *taken == reading.taken_at => *seen,
                    _ => now,
                };
                Received { reading, seen_at: seen_at.min(first) }
            }).collect();
            let peer = Peer { machine_id: roster.machine_id.clone(), machine: roster.machine, written_at, accounts: roster.accounts, run: roster.run, readings, listed_at: now };
            match peers.get(&roster.machine_id) {
                Some(p) if p.written_at >= peer.written_at => {}
                _ => { peers.insert(roster.machine_id, peer); }
            }
        }
        // A machine missing from this read that the last ones found is kept a while.
        for p in &self.seen.peers {
            if !peers.contains_key(&p.machine_id) && now - p.listed_at < Duration::minutes(KEEP_MISSING_MINUTES) {
                peers.insert(p.machine_id.clone(), p.clone());
            }
        }
        let mut peers: Vec<Peer> = peers.into_values().collect();
        peers.sort_by(|a, b| b.written_at.cmp(&a.written_at).then_with(|| a.machine.cmp(&b.machine)));
        self.rosters_seen = rosters_seen;
        self.first_seen = peers.iter()
            .flat_map(|p| p.readings.iter().map(|r| ((p.machine_id.clone(), r.reading.provider, r.reading.id.clone()), (r.reading.taken_at, r.seen_at))))
            .collect();
        // Once this run has written its roster, the copy there must be that one: if not, or if the
        // listing (which worked) had none, the next sync writes it again. A copy listed but not read
        // says nothing either way. The latest read that saw it decides.
        self.restore = match own {
            Own::Absent | Own::Other => self.written_hash.is_some(),
            Own::Unread => self.restore,
            Own::Written => false,
        };
        self.seen = Seen { own_accounts, peers, unreadable, unsupported, ..self.seen.clone() };
        (own_run, used)
    }
}

/// From this many rewrites of this machine's roster found not as written (`Seen::own_rewritten`),
/// Health says something else writes it.
pub const OWN_REWRITTEN_WARN: usize = 3;

/// A run going on whose machine has not rewritten its roster for this long has gone quiet (its
/// SlopBar stopped, or lost the sharing location); one rewrites every few refreshes at most.
const QUIET_MINUTES: i64 = 20;
/// A finished run is shown for this long after its last change.
const SHOW_FINISHED_MINUTES: i64 = 60;

/// Another machine's run as Settings > Accounts shows it: "Signing in on <machine>".
#[derive(Debug, Clone, Serialize)]
pub struct PeerRun {
    pub machine_id: String,
    pub machine: String,
    pub written_at: DateTime<Utc>,
    pub run: Run,
    /// Going on, but nothing heard from its machine for a while.
    pub quiet: bool,
}

/// The other machines' runs worth showing: going on, or finished within the hour.
pub fn peer_runs(seen: &Seen, now: DateTime<Utc>) -> Vec<PeerRun> {
    seen.peers.iter()
        .filter_map(|p| {
            let run = p.run.clone()?;
            // A clock running ahead counts as now.
            let at = run.at.min(now);
            (!run.finished || now - at < Duration::minutes(SHOW_FINISHED_MINUTES)).then(|| PeerRun {
                machine_id: p.machine_id.clone(), machine: p.machine.clone(), written_at: p.written_at,
                quiet: !run.finished && now - p.written_at > Duration::minutes(QUIET_MINUTES), run,
            })
        })
        .collect()
}

/// The accounts other machines' rosters name that are not signed in here: once each, newest
/// roster first, with the first email any of them gives.
pub fn waiting(seen: &Seen, here: &[AccountUsage]) -> Vec<Pending> {
    let mut out: Vec<Pending> = Vec::new();
    for e in seen.peers.iter().flat_map(|p| &p.accounts) {
        if here.iter().any(|a| a.provider == e.provider && a.id == e.id) {
            continue;
        }
        match out.iter_mut().find(|p| p.provider == e.provider && p.id == e.id) {
            Some(p) => { if p.email.is_none() { p.email = e.email.clone(); } }
            None => out.push(Pending { provider: e.provider, id: e.id.clone(), email: e.email.clone() }),
        }
    }
    out
}

/// Another machine's newest reading of each account, from the rosters last read: shown for an
/// account without a fresh reading here when it is newer than this machine's own by more than
/// `margin` (a refresh: so a reading here that failed once does not swap in another for a moment)
/// and under `SHARED_MAX_DAYS` old (`AccountUsage::shared`), and naming the machine covering the
/// account while it is under `COVERED_HOURS` old (`covered_by`), fresh here or not. A fresh
/// reading here always wins, and nothing else here goes by another machine's. Each is dated from
/// when this machine first saw it at the latest, so one that stops moving ages here, however it
/// is dated. `me` is this machine's name: a machine going by it is left out, since its reading
/// would pass for this machine's own.
pub fn apply_readings(list: &mut [AccountUsage], seen: &Seen, me: &str, margin: Duration, now: DateTime<Utc>) {
    for a in list.iter_mut() {
        a.shared = None;
        a.covered_by = None;
        let readings: Vec<_> = seen.peers.iter()
            .filter(|p| me.is_empty() || !p.machine.eq_ignore_ascii_case(me))
            .flat_map(|p| p.readings.iter().filter(|r| r.reading.provider == a.provider && r.reading.id == a.id).map(move |r| (p, r)))
            .map(|(p, r)| (p, r, r.reading.taken_at.min(r.seen_at).min(now)))
            .filter(|(_, _, at)| now - *at <= Duration::days(SHARED_MAX_DAYS))
            .collect();
        // Covered by a machine whose sign-in for it has not lapsed and whose reading is recent.
        a.covered_by = readings.iter()
            .filter(|(_, r, at)| !r.reading.lapsed && now - *at <= Duration::hours(COVERED_HOURS))
            .max_by_key(|(p, _, at)| (*at, p.written_at))
            .map(|(p, _, _)| p.machine.clone());
        let Some((p, r, at)) = readings.into_iter().max_by_key(|(p, _, at)| (*at, p.written_at)) else { continue };
        if a.stale && a.fetched_at.is_none_or(|own| at > own + margin) {
            a.shared = Some(SharedReading { machine: p.machine.clone(), taken_at: at, windows: r.reading.windows.clone() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    const MACHINE_A: &str = "11111111-1111-4111-8111-111111111111";
    const MACHINE_B: &str = "22222222-2222-4222-8222-222222222222";

    fn pass(s: &str) -> Passphrase { Passphrase(s.into()) }

    fn account(provider: &str, id: &str) -> AccountUsage {
        serde_json::from_value(serde_json::json!({
            "id": id, "provider": provider, "email": format!("{id}@example.invalid"), "alias": "Synthetic Alias", "plan": "max",
            "org": "Synthetic Org", "active": false, "sources": [], "windows": [], "credits": null, "resets": [], "error": null,
            "stale": false, "fetched_at": null, "monitor_connected": true,
        })).unwrap()
    }

    fn now() -> DateTime<Utc> { "2026-10-23T12:00:00Z".parse().unwrap() }

    #[test]
    fn a_roster_holds_only_provider_id_email_and_plan_encrypted() {
        let roster = own(MACHINE_A, "DESK-1", &[account("codex", "cx-one"), account("claude", "cl-one"), account("claude", "cl-one")], now());
        assert_eq!(roster.accounts.iter().map(|e| (e.provider.key(), e.id.as_str())).collect::<Vec<_>>(), [("claude", "cl-one"), ("codex", "cx-one")]);
        let json = serde_json::to_value(&roster.accounts[0]).unwrap();
        assert_eq!(json, serde_json::json!({ "provider": "claude", "id": "cl-one", "email": "cl-one@example.invalid", "plan": "max" }));
        let key = SealKey::new(&pass("correct horse battery")).unwrap();
        let bytes = seal(&roster, &key).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        // The plan as the roster's JSON writes it, quotes and all: the three letters alone turn up in
        // the base64 ciphertext by chance (about once in 700 runs), a quote never does.
        for secret in ["cl-one", "example.invalid", "DESK-1", "Synthetic", "\"max\""] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        let mut keys = HashMap::new();
        let back = open(&bytes, &pass("correct horse battery"), &mut keys).unwrap();
        assert_eq!(back.accounts, roster.accounts);
        assert_eq!(back.machine, "DESK-1");
        assert_eq!(keys.len(), 1, "the key is kept for the next read");
        assert!(matches!(open(&bytes, &pass("wrong horse battery"), &mut HashMap::new()), Err(OpenError::WrongOrDamaged)));
        let mut changed = bytes.clone();
        let at = text.find("\"ciphertext\": \"").unwrap() + 20;
        changed[at] = if changed[at] == b'A' { b'B' } else { b'A' };
        assert!(matches!(open(&changed, &pass("correct horse battery"), &mut HashMap::new()), Err(OpenError::WrongOrDamaged)));
        assert!(matches!(open(b"{\"format\":\"slopbar-backup\"}", &pass("correct horse battery"), &mut HashMap::new()), Err(OpenError::NotOurs)));
    }

    #[test]
    fn a_new_machine_finds_every_account_its_other_machines_have() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        // The old machine has three accounts; the new one has signed in one of them.
        let mut old = Syncer::default();
        old.sync(&store, &p, (MACHINE_A, "OLD-PC"), &[account("claude", "cl-one"), account("codex", "cx-one"), account("claude", "cl-two")], now(), false, false);
        assert_eq!(old.seen.wrote, Some(true));
        assert_eq!(old.seen.own_accounts, Some(3), "its own roster reads back");
        assert!(shared.0.join("SlopBar").join("roster").join(format!("{MACHINE_A}.roster")).is_file());
        let here = [account("claude", "cl-two")];
        let mut new = Syncer::default();
        new.sync(&store, &p, (MACHINE_B, "NEW-PC"), &here, now(), false, false);
        assert_eq!(new.seen.peers.iter().map(|p| p.machine.as_str()).collect::<Vec<_>>(), ["OLD-PC"]);
        let pending = waiting(&new.seen, &here);
        assert_eq!(pending.iter().map(|p| (p.provider.key(), p.id.as_str(), p.email.as_deref())).collect::<Vec<_>>(),
            [("claude", "cl-one", Some("cl-one@example.invalid")), ("codex", "cx-one", Some("cx-one@example.invalid"))],
            "signed-in accounts are not pending");
        // Its plan shows them as Add steps, one email at a time.
        let snapshot = crate::model::Snapshot { accounts: here.to_vec(), updated_at: None };
        let plan = crate::bulk::sign_in_plan(&snapshot, &pending, false);
        assert_eq!(plan.iter().map(|g| g.steps.len()).sum::<usize>(), 2);
        // Once signed in here, nothing is waiting.
        let all = [account("claude", "cl-one"), account("codex", "cx-one"), account("claude", "cl-two")];
        assert!(waiting(&new.seen, &all).is_empty());
    }

    #[test]
    fn another_passphrase_is_counted_and_old_rosters_are_ignored() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let mut a = Syncer::default();
        a.sync(&store, &pass("synthetic passphrase one"), (MACHINE_A, "A"), &[account("claude", "cl-one")], now(), false, false);
        let mut b = Syncer::default();
        b.sync(&store, &pass("synthetic passphrase two"), (MACHINE_B, "B"), &[], now(), false, false);
        assert!(b.seen.peers.is_empty());
        assert_eq!(b.seen.unreadable, 1, "a roster sealed with another passphrase is counted, not read");
        let mut late = Syncer::default();
        late.sync(&store, &pass("synthetic passphrase one"), (MACHINE_B, "B"), &[], now() + Duration::days(IGNORE_AFTER_DAYS + 1), false, false);
        assert!(late.seen.peers.is_empty(), "a roster unwritten for two months is from a machine no longer used");
    }

    /// Counts the writes, to show the roster is written only when it changes.
    struct Counting(Folder, std::sync::atomic::AtomicUsize);

    impl RosterStore for Counting {
        fn listing(&self, known: Known) -> std::io::Result<Listed> { self.0.listing(known) }
        fn put(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.0.put(name, bytes)
        }
        fn remove(&self, name: &str) -> std::io::Result<()> { self.0.remove(name) }
    }

    #[test]
    fn writes_when_accounts_change_or_a_day_passes_and_withdraws() {
        let shared = TempDir::new();
        let store = Counting(Folder(shared.0.clone()), Default::default());
        let writes = || store.1.load(std::sync::atomic::Ordering::SeqCst);
        let p = pass("synthetic roster passphrase");
        let mut sync = Syncer::default();
        let one = [account("claude", "cl-one")];
        sync.sync(&store, &p, (MACHINE_A, "A"), &one, now(), false, false);
        sync.sync(&store, &p, (MACHINE_A, "A"), &one, now() + Duration::minutes(5), false, false);
        assert_eq!(writes(), 1, "unchanged: not written again");
        sync.sync(&store, &p, (MACHINE_A, "A"), &[account("claude", "cl-one"), account("codex", "cx-one")], now() + Duration::minutes(10), false, false);
        assert_eq!(writes(), 2);
        assert_eq!(sync.seen.own_accounts, Some(2), "read again after a write");
        sync.sync(&store, &p, (MACHINE_A, "A"), &[account("claude", "cl-one"), account("codex", "cx-one")], now() + Duration::hours(25), false, false);
        assert_eq!(writes(), 3, "rewritten daily, so it never looks abandoned");
        store.remove(&file_name(MACHINE_A).unwrap()).unwrap();
        store.remove(&file_name(MACHINE_A).unwrap()).unwrap();
        assert!(store.list().unwrap().is_empty(), "withdrawn, and withdrawing twice is fine");
    }

    #[test]
    fn the_passphrase_is_kept_here_and_never_too_short() {
        let dir = TempDir::new();
        assert!(load_passphrase(&dir.0).is_none());
        assert_eq!(store_passphrase(&dir.0, Some(&pass("short"))), Err("roster_passphrase_too_short".to_string()));
        store_passphrase(&dir.0, Some(&pass("synthetic roster passphrase"))).unwrap();
        assert_eq!(load_passphrase(&dir.0).map(|p| p.0), Some("synthetic roster passphrase".into()));
        #[cfg(windows)]
        assert!(!String::from_utf8_lossy(&std::fs::read(passphrase_path(&dir.0)).unwrap()).contains("synthetic"), "DPAPI, not plaintext");
        store_passphrase(&dir.0, None).unwrap();
        store_passphrase(&dir.0, None).unwrap();
        assert!(load_passphrase(&dir.0).is_none());
    }

    fn run(revision: u64, finished: bool, state: &str) -> Run {
        Run { revision, at: now(), total: 2, done: 0, emails: 1, email: 1, finished, still_waiting: false, monitoring: true, everywhere: false,
            error: finished.then(|| "signin_stopped".into()),
            steps: vec![
                RunStep { provider: Provider::Claude, account: Some("cl-one@example.invalid".into()), kind: "add".into(), state: state.into(), error: None },
                RunStep { provider: Provider::Codex, account: Some("cl-one@example.invalid".into()), kind: "connect".into(), state: "pending".into(), error: None },
            ] }
    }

    #[test]
    fn a_run_reaches_the_other_machines_encrypted_newest_first() {
        let shared = TempDir::new();
        let store = Counting(Folder(shared.0.clone()), Default::default());
        let writes = || store.1.load(std::sync::atomic::Ordering::SeqCst);
        let p = pass("synthetic roster passphrase");
        let one = [account("claude", "cl-two")];
        let mut target = Syncer::default();
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, now(), false, false);
        assert_eq!(writes(), 1);
        // A step changes: written at once, though the accounts did not change.
        target.offer_run(run(3, false, "running"));
        target.offer_run(run(2, false, "pending"));
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, now() + Duration::seconds(5), false, false);
        assert_eq!(writes(), 2, "an older revision offered later is ignored, the newer one written");
        let bytes = std::fs::read(shared.0.join("SlopBar").join("roster").join(format!("{MACHINE_A}.roster"))).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("example.invalid"), "the run is inside the encrypted roster");
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, now() + Duration::seconds(10), false, false);
        assert_eq!(writes(), 2, "unchanged: not written again");
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, now() + Duration::minutes(RUN_HEARTBEAT_MINUTES) + Duration::seconds(6), false, false);
        assert_eq!(writes(), 3, "rewritten every few minutes while it runs, so it never looks quiet");
        // The pilot sees it, and reads again soon while it runs.
        let mut pilot = Syncer::default();
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, now() + Duration::minutes(4), false, false);
        let peer = &pilot.seen.peers[0];
        assert_eq!((peer.machine_id.as_str(), peer.machine.as_str()), (MACHINE_A, "TARGET"));
        let seen = peer.run.clone().unwrap();
        assert_eq!((seen.revision, seen.steps[0].state.as_str(), seen.steps[0].account.as_deref()), (3, "running", Some("cl-one@example.invalid")));
        target.offer_run(run(4, true, "failed"));
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, now() + Duration::minutes(4), false, false);
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, now() + Duration::minutes(4) + Duration::seconds(READ_LIVE_SECONDS), false, false);
        let seen = pilot.seen.peers[0].run.clone().unwrap();
        assert_eq!((seen.finished, seen.error.as_deref()), (true, Some("signin_stopped")), "read again within seconds while a run went on");
        // Finished: back to the slow pace, unless Settings is watching.
        let read_at = pilot.seen.read_at;
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, now() + Duration::minutes(5), false, false);
        assert_eq!(pilot.seen.read_at, read_at);
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, now() + Duration::minutes(5), false, true);
        assert_ne!(pilot.seen.read_at, read_at);
        // Shown for an hour after it finished, then no more.
        assert_eq!(peer_runs(&pilot.seen, now() + Duration::minutes(5)).len(), 1);
        assert!(peer_runs(&pilot.seen, now() + Duration::minutes(SHOW_FINISHED_MINUTES)).is_empty());
    }

    #[test]
    fn after_a_retry_the_machine_is_read_at_the_live_pace_until_its_run_changes() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        let one = [account("claude", "cl-two")];
        let at = |s: i64| now() + Duration::seconds(s);
        let mut target = Syncer::default();
        target.offer_run(run(4, true, "failed"));
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, at(0), false, false);
        let mut pilot = Syncer::default();
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, at(60), false, false);
        // Nothing going on: the next read waits, even with Settings watching.
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, at(60 + READ_LIVE_SECONDS), false, true);
        assert_eq!(pilot.seen.read_at, Some(at(60)));
        // Retry sent (the target has not uploaded its new run yet): read again within seconds.
        pilot.expect_news(MACHINE_A, at(62));
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, at(60 + READ_LIVE_SECONDS), false, false);
        assert_eq!(pilot.seen.read_at, Some(at(60 + READ_LIVE_SECONDS)), "live pace, Settings closed");
        target.offer_run(Run { at: at(80), ..run(5, false, "running") });
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, at(80), false, false);
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, at(60 + 2 * READ_LIVE_SECONDS), false, false);
        assert_eq!(pilot.seen.peers[0].run.as_ref().map(|r| (r.revision, r.finished)), Some((5, false)));
        target.offer_run(Run { at: at(95), ..run(6, true, "failed") });
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, at(95), false, false);
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, at(60 + 3 * READ_LIVE_SECONDS), false, false);
        // The news came: back to the slow pace.
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, at(60 + 4 * READ_LIVE_SECONDS), false, false);
        assert_eq!(pilot.seen.read_at, Some(at(60 + 3 * READ_LIVE_SECONDS)));
        // No news at all: the wait runs out.
        let mut other = Syncer::default();
        other.sync(&store, &p, (MACHINE_B, "PILOT"), &one, at(200), false, false);
        other.expect_news(MACHINE_A, at(200));
        other.sync(&store, &p, (MACHINE_B, "PILOT"), &one, at(200 + EXPECT_MINUTES * 60), false, false);
        assert_eq!(other.seen.read_at, Some(at(200)));
    }

    #[test]
    fn a_failed_run_outlives_a_restart_of_its_app_for_the_hour() {
        let p = pass("synthetic roster passphrase");
        let one = [account("claude", "cl-two")];
        let after = |m: i64| now() + Duration::minutes(m);
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let mut target = Syncer::default();
        target.offer_run(run(4, true, "failed"));
        target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, after(0), false, false);
        // Restarted: its first write keeps the run, so the pilot keeps its Retry.
        let mut restarted = Syncer::default();
        restarted.sync(&store, &p, (MACHINE_A, "TARGET"), &one, after(10), false, false);
        assert_eq!(restarted.seen.own_accounts, Some(1));
        let mut pilot = Syncer::default();
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, after(11), false, false);
        let shown = peer_runs(&pilot.seen, after(11));
        assert_eq!(shown.iter().map(|r| (r.run.revision, r.run.error.as_deref())).collect::<Vec<_>>(), [(4, Some("signin_stopped"))]);
        // Its next run starts again at revision 1, and replaces it.
        restarted.offer_run(run(1, false, "running"));
        restarted.sync(&store, &p, (MACHINE_A, "TARGET"), &one, after(12), false, false);
        pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, after(12), true, false);
        assert_eq!(pilot.seen.peers[0].run.as_ref().map(|r| r.revision), Some(1));
        // A run older than the hour, or one that went well, is not taken back.
        for (old, minutes) in [(run(4, true, "failed"), SHOW_FINISHED_MINUTES), (Run { error: None, ..run(4, true, "done") }, 10)] {
            let shared = TempDir::new();
            let store = Folder(shared.0.clone());
            let mut target = Syncer::default();
            target.offer_run(old);
            target.sync(&store, &p, (MACHINE_A, "TARGET"), &one, after(0), false, false);
            let mut restarted = Syncer::default();
            restarted.sync(&store, &p, (MACHINE_A, "TARGET"), &one, after(minutes), false, false);
            let mut pilot = Syncer::default();
            pilot.sync(&store, &p, (MACHINE_B, "PILOT"), &one, after(minutes), false, false);
            assert!(pilot.seen.peers[0].run.is_none(), "{minutes}");
        }
    }

    #[test]
    fn a_run_whose_machine_went_quiet_says_so() {
        let peer = |written: i64, finished: bool| Peer { machine_id: MACHINE_A.into(), machine: "TARGET".into(),
            written_at: now() - Duration::minutes(written), accounts: vec![], run: Some(run(1, finished, "running")), readings: vec![], listed_at: now() };
        let seen = |p: Peer| Seen { peers: vec![p], ..Seen::default() };
        assert!(!peer_runs(&seen(peer(1, false)), now())[0].quiet);
        assert!(peer_runs(&seen(peer(QUIET_MINUTES + 1, false)), now())[0].quiet, "no word for a while");
        assert!(peer_runs(&seen(Peer { run: None, ..peer(1, false) }), now()).is_empty(), "no run, no card");
    }

    #[test]
    fn a_run_from_another_machine_is_trimmed_to_known_words() {
        let mut roster = own(MACHINE_A, "A", &[], now());
        let mut r = run(1, false, "running");
        r.steps.push(RunStep { provider: Provider::Claude, account: None, kind: "delete".into(), state: "running".into(), error: None });
        r.steps.push(RunStep { provider: Provider::Claude, account: None, kind: "add".into(), state: "hacked".into(), error: None });
        r.error = Some("bad\nerror".into());
        roster.run = Some(r);
        let key = SealKey::new(&pass("synthetic roster passphrase")).unwrap();
        let back = open(&seal(&roster, &key).unwrap(), &pass("synthetic roster passphrase"), &mut HashMap::new()).unwrap().run.unwrap();
        assert_eq!(back.steps.len(), 2);
        assert_eq!(back.error.as_deref(), Some("unknown"));
        // A roster from a SlopBar without runs still opens.
        let mut plain: serde_json::Value = serde_json::to_value(own(MACHINE_A, "A", &[], now())).unwrap();
        plain.as_object_mut().unwrap().remove("run");
        let bytes = crate::backup::seal_bytes(FORMAT, &serde_json::to_vec(&plain).unwrap(), &key).unwrap();
        assert!(open(&bytes, &pass("synthetic roster passphrase"), &mut HashMap::new()).unwrap().run.is_none());
    }

    /// An account as a refresh leaves it: read at `at` (minutes from `now()`) with its weekly
    /// window at `percent`, or never read; `stale` when this refresh's read failed.
    fn reading(provider: &str, id: &str, percent: f64, at: Option<i64>, stale: bool) -> AccountUsage {
        let mut a = account(provider, id);
        a.windows = vec![UsageWindow { kind: "weekly".into(), window_seconds: None, used_percent: percent, resets_at: Some("2026-10-27T09:00:00Z".parse().unwrap()) }];
        a.fetched_at = at.map(|m| now() + Duration::minutes(m));
        if at.is_none() {
            a.windows.clear();
        }
        a.stale = stale;
        a
    }

    fn shared_percent(a: &AccountUsage) -> Option<(String, f64)> {
        a.shared.as_ref().map(|s| (s.machine.clone(), s.windows[0].used_percent))
    }

    fn roster_file(dir: &TempDir, machine_id: &str) -> PathBuf {
        dir.0.join("SlopBar").join("roster").join(file_name(machine_id).unwrap())
    }

    /// A refresh: another machine's reading must be newer than this one's own by more than this.
    fn margin() -> Duration { Duration::minutes(5) }

    #[test]
    fn a_reading_reaches_the_other_machines_encrypted_and_is_never_passed_on() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        // A reads both accounts itself.
        let mut a = Syncer::default();
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &[reading("claude", "cl-one", 37.0, Some(0), false), reading("codex", "cx-one", 81.0, Some(0), false)], now(), false, false);
        let bytes = std::fs::read(roster_file(&shared, MACHINE_A)).unwrap();
        for plain in ["weekly", "used_percent", "37.0", "taken_at"] {
            assert!(!String::from_utf8_lossy(&bytes).contains(plain), "{plain}: readings are inside the encrypted roster");
        }
        // B's monitoring for cl-one ended days ago: it shows A's reading, marked as A's. cx-one is
        // in use on B and read through its CLI: B's own reading shows, and A's still covers it.
        let mut here = vec![reading("claude", "cl-one", 12.0, Some(-3 * 24 * 60), true), reading("codex", "cx-one", 80.0, Some(1), false)];
        let mut b = Syncer::default();
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &here, now() + Duration::minutes(2), false, false);
        apply_readings(&mut here, &b.seen, "LAPTOP-B", margin(), now() + Duration::minutes(2));
        assert_eq!(shared_percent(&here[0]), Some(("DESK-A".into(), 37.0)));
        assert_eq!(here[0].shared.as_ref().unwrap().taken_at, now());
        assert_eq!(here[0].windows[0].used_percent, 12.0, "its own reading stays its own");
        assert!(here[0].stale, "and still stale: shown only");
        assert!(here[1].shared.is_none(), "a fresh reading here always wins");
        assert_eq!((here[0].covered_by.as_deref(), here[1].covered_by.as_deref()), (Some("DESK-A"), Some("DESK-A")), "covered either way");
        // B writes its roster with the shared reading in its snapshot: only its own readings go.
        let mut again = Syncer::default();
        let mut only_shared = reading("claude", "cl-two", 0.0, None, true);
        only_shared.shared = here[0].shared.clone();
        again.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[here[0].clone(), here[1].clone(), only_shared], now() + Duration::minutes(3), true, false);
        let written = open(&std::fs::read(roster_file(&shared, MACHINE_B)).unwrap(), &p, &mut HashMap::new()).unwrap();
        assert_eq!(written.readings.iter().map(|r| (r.id.as_str(), r.windows[0].used_percent, r.taken_at)).collect::<Vec<_>>(),
            [("cl-one", 12.0, now() - Duration::days(3)), ("cx-one", 80.0, now() + Duration::minutes(1))],
            "A's reading is not passed on as B's, and an account with only A's reading has none to give");
        // With sharing readings off, B's roster goes without them, at once; A's are still shown.
        let mut off = Syncer { withhold_readings: true, ..Syncer::default() };
        off.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &here, now() + Duration::minutes(4), true, false);
        assert!(open(&std::fs::read(roster_file(&shared, MACHINE_B)).unwrap(), &p, &mut HashMap::new()).unwrap().readings.is_empty());
        let mut shown = here.clone();
        apply_readings(&mut shown, &off.seen, "LAPTOP-B", margin(), now() + Duration::minutes(4));
        assert_eq!(shared_percent(&shown[0]), Some(("DESK-A".into(), 37.0)));
    }

    fn received(percent: f64, taken: DateTime<Utc>, seen: DateTime<Utc>) -> Received {
        Received {
            reading: Reading { lapsed: false, provider: Provider::Claude, id: "cl-one".into(), taken_at: taken,
                windows: vec![UsageWindow { kind: "weekly".into(), window_seconds: None, used_percent: percent, resets_at: None }] },
            seen_at: seen,
        }
    }

    fn peer_with(id: &str, machine: &str, readings: Vec<Received>) -> Peer {
        let written_at = readings.iter().map(|r| r.reading.taken_at).max().unwrap_or(now());
        Peer { machine_id: id.into(), machine: machine.into(), written_at, accounts: vec![], run: None, readings, listed_at: now() }
    }

    #[test]
    fn the_newest_reading_wins_and_never_one_not_newer_than_this_machines_own() {
        let at = |m: i64| now() + Duration::minutes(m);
        let seen = Seen { peers: vec![peer_with(MACHINE_A, "OLDER", vec![received(20.0, at(5), at(6))]), peer_with(MACHINE_B, "NEWER", vec![received(30.0, at(10), at(11))])], ..Seen::default() };
        let mut list = [reading("claude", "cl-one", 10.0, None, true), reading("claude", "cl-one", 10.0, Some(4), true), reading("claude", "cl-one", 10.0, Some(6), true),
            reading("claude", "cl-one", 10.0, Some(12), true), reading("claude", "cl-one", 10.0, Some(0), false), reading("claude", "cl-two", 10.0, None, true)];
        apply_readings(&mut list, &seen, "HERE", margin(), at(15));
        assert_eq!(list.iter().map(shared_percent).collect::<Vec<_>>(), [
            Some(("NEWER".into(), 30.0)), // never read here
            Some(("NEWER".into(), 30.0)), // read here a refresh and more before
            None,                         // read here under a refresh before: its own stays
            None,                         // read here after both, though it failed since
            None,                         // read here just now, though older than both
            None,                         // no other machine has read it
        ]);
        assert_eq!(list.iter().map(|a| a.covered_by.as_deref()).collect::<Vec<_>>(),
            [Some("NEWER"), Some("NEWER"), Some("NEWER"), Some("NEWER"), Some("NEWER"), None], "covered whatever its own reading");
        // Under the same name as this machine, a reading would pass for its own: not shown.
        apply_readings(&mut list, &seen, "newer", margin(), at(15));
        assert_eq!(shared_percent(&list[0]), Some(("OLDER".into(), 20.0)));
        // A week old: shown no more, nor covering.
        apply_readings(&mut list, &seen, "HERE", margin(), at(10) + Duration::days(SHARED_MAX_DAYS) + Duration::minutes(1));
        assert!(list.iter().all(|a| a.shared.is_none() && a.covered_by.is_none()));
    }

    #[test]
    fn a_reading_that_stops_moving_ages_here_and_stops_covering() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        let hours = |h: i64| now() + Duration::hours(h);
        // A machine that stopped reading (its own sign-in ended, or it is off): its last reading
        // covers for COVERED_HOURS, then ages on.
        let key = SealKey::new(&p).unwrap();
        let mut frozen = own(MACHINE_A, "DESK-A", &[account("claude", "cl-one")], hours(0));
        frozen.readings = vec![Reading { lapsed: false, provider: Provider::Claude, id: "cl-one".into(), taken_at: hours(0),
            windows: vec![UsageWindow { kind: "weekly".into(), window_seconds: None, used_percent: 5.0, resets_at: None }] }];
        store.put(&file_name(MACHINE_A).unwrap(), &seal(&frozen, &key).unwrap()).unwrap();
        let mut b = Syncer::default();
        let mut here = vec![reading("claude", "cl-one", 50.0, None, true)];
        let check = |b: &mut Syncer, here: &mut Vec<AccountUsage>, at: DateTime<Utc>| {
            b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), here, at, true, false);
            apply_readings(here, &b.seen, "LAPTOP-B", margin(), at);
            (here[0].covered_by.is_some(), here[0].shared.as_ref().map(|s| s.taken_at))
        };
        assert_eq!(check(&mut b, &mut here, hours(1)), (true, Some(hours(0))));
        assert_eq!(check(&mut b, &mut here, hours(COVERED_HOURS) + Duration::minutes(1)), (false, Some(hours(0))), "not moved for 12 hours: covers no more");
        // A roster dated ahead of this clock: its readings are not taken at all (the storage's
        // time would date them now at every read), its accounts are.
        let ahead = Roster { written_at: hours(48), readings: vec![Reading { taken_at: hours(48), ..frozen.readings[0].clone() }], ..frozen.clone() };
        store.put(&file_name(MACHINE_A).unwrap(), &seal(&ahead, &key).unwrap()).unwrap();
        let mut c = Syncer::default();
        let mut there = vec![reading("claude", "cl-one", 50.0, None, true)];
        assert_eq!(check(&mut c, &mut there, hours(1)), (false, None));
        assert_eq!(c.seen.peers[0].accounts.len(), 1);
        // Its date comes: its readings count from when this machine first saw it, so they wake up
        // old, not fresh, and cover nothing.
        assert_eq!(check(&mut c, &mut there, hours(48)), (false, Some(hours(1))));
        // A roster dated a few minutes ahead (within what is trusted): dated from when it was seen.
        let soon = Roster { written_at: hours(50) + Duration::minutes(4),
            readings: vec![Reading { taken_at: hours(50) + Duration::minutes(4), ..frozen.readings[0].clone() }], ..frozen.clone() };
        store.put(&file_name(MACHINE_A).unwrap(), &seal(&soon, &key).unwrap()).unwrap();
        assert_eq!(check(&mut c, &mut there, hours(50)), (true, Some(hours(50))));
        // A reading that moves on covers again, dated as it says.
        let moved = Roster { written_at: hours(14), readings: vec![Reading { taken_at: hours(14), ..frozen.readings[0].clone() }], ..frozen };
        store.put(&file_name(MACHINE_A).unwrap(), &seal(&moved, &key).unwrap()).unwrap();
        assert_eq!(check(&mut b, &mut here, hours(14) + Duration::minutes(2)), (true, Some(hours(14))));
    }

    #[test]
    fn another_file_cannot_stand_in_for_a_machine_nor_a_name_hide_characters() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        let key = SealKey::new(&p).unwrap();
        let mut a = own(MACHINE_A, " DESK\u{200B}-A\u{202E}\u{FE0F}\u{34F}\u{E0041}\u{2028}\u{2029} ", &[account("claude", "cl-one")], now());
        a.readings = vec![Reading { lapsed: false, provider: Provider::Claude, id: "cl-one".into(), taken_at: now(),
            windows: vec![UsageWindow { kind: "weekly".into(), window_seconds: None, used_percent: 5.0, resets_at: None }] }];
        store.put(&file_name(MACHINE_A).unwrap(), &seal(&a, &key).unwrap()).unwrap();
        // A second file claiming A's id, newer, as a forger or a sync app's conflicted copy would leave.
        let forged = Roster { written_at: now() + Duration::minutes(1), readings: vec![Reading { taken_at: now() + Duration::minutes(1), ..a.readings[0].clone() }], ..a.clone() };
        store.put(&format!("{MACHINE_A} (conflicted copy).roster"), &seal(&forged, &key).unwrap()).unwrap();
        let mut b = Syncer::default();
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[], now() + Duration::minutes(2), false, false);
        assert_eq!(b.seen.peers.len(), 1);
        assert_eq!((b.seen.peers[0].machine.as_str(), b.seen.peers[0].readings[0].reading.taken_at), ("DESK-A", now()), "its own file, its name cleaned");
    }

    #[test]
    fn a_machine_missing_from_one_read_is_kept_a_while() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        let mut a = Syncer::default();
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &[reading("claude", "cl-one", 37.0, Some(0), false)], now(), false, false);
        let mut b = Syncer::default();
        let minutes = |m: i64| now() + Duration::minutes(m);
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[], minutes(1), false, false);
        let file = roster_file(&shared, MACHINE_A);
        let bytes = std::fs::read(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[], minutes(12), true, false);
        assert_eq!(b.seen.peers.len(), 1, "one read without it: kept");
        assert_eq!(b.seen.peers[0].readings.len(), 1);
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[], minutes(1 + KEEP_MISSING_MINUTES), true, false);
        assert!(b.seen.peers.is_empty(), "gone for longer: dropped");
        // Back after it was dropped: seen anew.
        std::fs::write(&file, bytes).unwrap();
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[], minutes(30), true, false);
        assert_eq!(b.seen.peers[0].readings[0].seen_at, minutes(30));
    }

    #[test]
    fn readings_are_written_when_they_change_at_most_every_gap_and_redated_by_the_heartbeat() {
        let shared = TempDir::new();
        let store = Counting(Folder(shared.0.clone()), Default::default());
        let writes = || store.1.load(std::sync::atomic::Ordering::SeqCst);
        let p = pass("synthetic roster passphrase");
        let mut sync = Syncer::default();
        let at = |m: i64| now() + Duration::minutes(m);
        let read = |percent: f64, m: i64| [reading("claude", "cl-one", percent, Some(m), false)];
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(10.0, 0), at(0), false, false);
        assert_eq!(writes(), 1);
        // Read again unchanged, or changed by less than the flyout shows: nothing to write, the
        // gap since the last write over or not.
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(10.0, 5), at(5), false, false);
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(10.2, 16), at(16), false, false);
        let mut moved = read(10.0, 20);
        moved[0].windows[0].resets_at = moved[0].windows[0].resets_at.map(|t| t + Duration::seconds(30));
        sync.sync(&store, &p, (MACHINE_A, "A"), &moved, at(20), false, false);
        assert_eq!(writes(), 1, "the same reading, newly dated, waits for the heartbeat");
        // A change waits out the gap since the last write, then carries every change since.
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(14.0, 21), at(21), false, false);
        assert_eq!(writes(), 2);
        assert_eq!(sync.seen.read_at, Some(at(16)), "a write of readings alone is not read back");
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(15.0, 26), at(26), false, false);
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(16.0, 31), at(31), false, false);
        assert_eq!(writes(), 2, "coalesced: not a write per poll");
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(17.0, 36), at(21 + READINGS_GAP_MINUTES), false, false);
        assert_eq!(writes(), 3);
        let mut keys = HashMap::new();
        let file = || std::fs::read(roster_file(&shared, MACHINE_A)).unwrap();
        let back = open(&file(), &p, &mut keys).unwrap();
        assert_eq!((back.readings[0].windows[0].used_percent, back.readings[0].taken_at), (17.0, at(36)));
        // Unchanged for hours: re-dated by the heartbeat, so the others can tell it still holds.
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(17.0, 300), at(300), false, false);
        assert_eq!(writes(), 3);
        let beat = 36 + READINGS_HEARTBEAT_HOURS * 60;
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(17.0, beat), at(beat), false, false);
        assert_eq!(writes(), 4);
        assert_eq!(open(&file(), &p, &mut keys).unwrap().readings[0].taken_at, at(beat));
        // Readings no longer taken here (its own sign-in ended too): nothing to re-date.
        sync.sync(&store, &p, (MACHINE_A, "A"), &read(17.0, beat), at(beat + 2 * READINGS_HEARTBEAT_HOURS * 60), false, false);
        assert_eq!(writes(), 4);
        // No readings at all (nothing read yet): no heartbeat either.
        let mut empty = Syncer::default();
        let dir = TempDir::new();
        let other = Counting(Folder(dir.0.clone()), Default::default());
        empty.sync(&other, &p, (MACHINE_A, "A"), &[account("claude", "cl-one")], at(0), false, false);
        empty.sync(&other, &p, (MACHINE_A, "A"), &[account("claude", "cl-one")], at(0) + Duration::hours(REWRITE_AFTER_HOURS - 1), false, false);
        assert_eq!(other.1.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A store whose writes fail, counting them.
    struct Failing(std::sync::atomic::AtomicUsize);

    impl RosterStore for Failing {
        fn listing(&self, _: Known) -> std::io::Result<Listed> { Ok(Listed::default()) }
        fn put(&self, _: &str, _: &[u8]) -> std::io::Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(std::io::Error::other("synthetic failure"))
        }
        fn remove(&self, _: &str) -> std::io::Result<()> { Ok(()) }
    }

    #[test]
    fn a_failed_write_of_readings_alone_waits_out_the_gap() {
        let shared = TempDir::new();
        let p = pass("synthetic roster passphrase");
        let at = |m: i64| now() + Duration::minutes(m);
        let read = |percent: f64, m: i64| [reading("claude", "cl-one", percent, Some(m), false)];
        let mut sync = Syncer::default();
        sync.sync(&Folder(shared.0.clone()), &p, (MACHINE_A, "A"), &read(10.0, 0), at(0), false, false);
        let failing = Failing(Default::default());
        let tries = || failing.0.load(std::sync::atomic::Ordering::SeqCst);
        for m in [READINGS_GAP_MINUTES, READINGS_GAP_MINUTES + 5, READINGS_GAP_MINUTES + 10] {
            sync.sync(&failing, &p, (MACHINE_A, "A"), &read(20.0 + m as f64, m), at(m), false, false);
        }
        assert_eq!(tries(), 1, "one try per gap, not one per refresh");
        sync.sync(&failing, &p, (MACHINE_A, "A"), &read(40.0, 2 * READINGS_GAP_MINUTES), at(2 * READINGS_GAP_MINUTES), false, false);
        assert_eq!(tries(), 2);
        // Accounts that changed are written at once, failure or not, as before.
        sync.sync(&failing, &p, (MACHINE_A, "A"), &[read(40.0, 31)[0].clone(), account("codex", "cx-one")], at(31), false, false);
        sync.sync(&failing, &p, (MACHINE_A, "A"), &[read(40.0, 32)[0].clone(), account("codex", "cx-one")], at(32), false, false);
        assert_eq!(tries(), 4);
    }

    #[test]
    fn a_day_of_codex_windows_not_started_costs_a_few_writes_and_one_in_use_a_write_per_gap() {
        // An idle Codex account: both windows unused, each reset one window length after the read,
        // a time that moves with every read. Polled every 5 minutes for a day.
        let unused = |m: i64| {
            let mut a = account("codex", "cx-idle");
            let read_at = now() + Duration::minutes(m);
            a.windows = vec![
                UsageWindow { kind: "session".into(), window_seconds: Some(18_000), used_percent: 0.0, resets_at: Some(read_at + Duration::seconds(18_000)) },
                UsageWindow { kind: "weekly".into(), window_seconds: Some(604_800), used_percent: 0.0, resets_at: Some(read_at + Duration::seconds(604_800)) },
            ];
            a.fetched_at = Some(read_at);
            a
        };
        let p = pass("synthetic roster passphrase");
        let day = |accounts: &dyn Fn(i64) -> Vec<AccountUsage>| {
            let shared = TempDir::new();
            let store = Counting(Folder(shared.0.clone()), Default::default());
            let mut sync = Syncer::default();
            for m in (0..24 * 60).step_by(5) {
                sync.sync(&store, &p, (MACHINE_A, "A"), &accounts(m as i64), now() + Duration::minutes(m as i64), false, false);
            }
            store.1.load(std::sync::atomic::Ordering::SeqCst)
        };
        let idle = day(&|m| vec![unused(m)]);
        assert!(idle <= 5, "{idle} writes for a day of nothing changing");
        let busy = day(&|m| vec![unused(m), reading("claude", "cl-busy", (m / 5) as f64 % 100.0, Some(m), false)]);
        assert!((90..=97).contains(&busy), "{busy} writes for a day of use, one per gap");
    }

    #[test]
    fn a_roster_stays_under_the_limit_dropping_its_oldest_readings_and_is_padded() {
        let key = SealKey::new(&pass("synthetic roster passphrase")).unwrap();
        // The file's size does not follow its readings' digits.
        let sized = |percent: f64| seal(&Roster { readings: readings(&[reading("claude", "cl-one", percent, Some(0), false)], now()), ..own(MACHINE_A, "A", &[account("claude", "cl-one")], now()) }, &key).unwrap().len();
        assert_eq!(sized(1.0), sized(12.345678));
        // 500 accounts with four windows each: too many readings for the limit; the oldest go.
        let many: Vec<AccountUsage> = (0..MAX_ACCOUNTS).map(|i| {
            let mut a = reading("claude", &format!("cl-{i:03}-{}", "x".repeat(60)), 50.0, Some(-(i as i64)), false);
            a.windows = ["session", "weekly", "weekly_opus", "weekly_sonnet"].iter()
                .map(|k| UsageWindow { kind: (*k).into(), window_seconds: None, used_percent: 50.0, resets_at: Some(now() + Duration::days(3)) }).collect();
            a
        }).collect();
        let mut roster = own(MACHINE_A, "A", &many, now());
        roster.readings = readings(&many, now());
        let bytes = seal(&roster, &key).unwrap();
        assert!(bytes.len() as u64 <= WRITE_LIMIT, "{}", bytes.len());
        let back = open(&bytes, &pass("synthetic roster passphrase"), &mut HashMap::new()).unwrap();
        assert_eq!(back.accounts.len(), MAX_ACCOUNTS, "every account stays");
        assert!(!back.readings.is_empty() && back.readings.len() < MAX_ACCOUNTS);
        let oldest_kept = back.readings.iter().map(|r| r.taken_at).min().unwrap();
        assert!(many.iter().filter(|a| a.fetched_at.unwrap() > oldest_kept).all(|a| back.readings.iter().any(|r| r.id == a.id)), "the newest are kept");
        // Readings older than a week, and of accounts known by their token, are not written.
        let token = reading("claude", "token:abc", 5.0, Some(0), false);
        let old = reading("claude", "cl-old", 5.0, Some(-(SHARED_MAX_DAYS * 24 * 60) - 1), false);
        assert!(readings(&[token, old], now()).is_empty());
    }

    #[test]
    fn readings_from_others_are_trimmed_to_windows_the_providers_give() {
        let window = |kind: &str, percent: f64, seconds: Option<i64>| UsageWindow { kind: kind.into(), window_seconds: seconds, used_percent: percent, resets_at: None };
        let mut roster = own(MACHINE_A, "A", &[account("claude", "cl-one"), account("codex", "cx-one")], now());
        let taken = |m: i64| now() - Duration::minutes(m);
        roster.readings = vec![
            Reading { lapsed: false, provider: Provider::Claude, id: "cl-one".into(), taken_at: taken(5), windows: vec![
                window("session", 12.0, None), window("<img src=x>", 50.0, None), window("constructor", 50.0, None), window("weekly", 5000.0, None), window("weekly", -1.0, None),
                window("window", 40.0, Some(-5)), window("window", 41.0, Some(18_000)), window("window", 42.0, Some(MAX_WINDOW_SECONDS + 1)),
                UsageWindow { resets_at: Some(taken(5) + Duration::days(40)), ..window("weekly", 43.0, None) },
            ] },
            // An older duplicate, a reading of an account the roster does not list, and one with no window left.
            Reading { lapsed: false, provider: Provider::Claude, id: "cl-one".into(), taken_at: taken(50), windows: vec![window("session", 99.0, None)] },
            Reading { lapsed: false, provider: Provider::Claude, id: "cl-unlisted".into(), taken_at: taken(5), windows: vec![window("session", 1.0, None)] },
            Reading { lapsed: false, provider: Provider::Codex, id: "cx-one".into(), taken_at: taken(5), windows: vec![window("bogus", 1.0, None)] },
        ];
        roster.readings.extend((0..MAX_WINDOWS + 3).map(|_| Reading { lapsed: false, provider: Provider::Codex, id: "cx-one".into(), taken_at: taken(60), windows: vec![window("session", 3.0, None); MAX_WINDOWS + 3] }));
        let key = SealKey::new(&pass("synthetic roster passphrase")).unwrap();
        let back = open(&seal(&roster, &key).unwrap(), &pass("synthetic roster passphrase"), &mut HashMap::new()).unwrap();
        let summary: Vec<_> = back.readings.iter().map(|r| (r.id.as_str(), r.taken_at, r.windows.iter().map(|w| (w.kind.as_str(), w.used_percent, w.window_seconds, w.resets_at)).collect::<Vec<_>>())).collect();
        assert_eq!(summary[0], ("cl-one", taken(5), vec![("session", 12.0, None, None), ("window", 40.0, None, None), ("window", 41.0, Some(18_000), None),
            ("window", 42.0, None, None), ("weekly", 43.0, None, None)]));
        assert_eq!((summary[1].0, summary[1].1, summary[1].2.len()), ("cx-one", taken(60), MAX_WINDOWS), "the newest with windows left, cut to the limit");
        assert_eq!(summary.len(), 2);
        // A reading this SlopBar cannot read (another provider, a number where text goes) is
        // dropped, not the roster with it.
        let mut odd: serde_json::Value = serde_json::to_value(&roster).unwrap();
        odd["readings"].as_array_mut().unwrap().insert(0, serde_json::json!({ "provider": "gemini", "id": "cl-one", "taken_at": taken(1), "windows": [] }));
        odd["readings"][1]["windows"][0]["used_percent"] = serde_json::json!("lots");
        let bytes = crate::backup::seal_bytes(FORMAT, &serde_json::to_vec(&odd).unwrap(), &key).unwrap();
        let back = open(&bytes, &pass("synthetic roster passphrase"), &mut HashMap::new()).unwrap();
        assert_eq!((back.accounts.len(), back.readings.iter().map(|r| (r.id.as_str(), r.taken_at)).collect::<Vec<_>>()), (2, vec![("cl-one", taken(50)), ("cx-one", taken(60))]));
        // A reading dated after its roster was written is dated when the roster was.
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let mut later = own(MACHINE_A, "A", &[account("claude", "cl-one")], now());
        later.readings = vec![Reading { lapsed: false, provider: Provider::Claude, id: "cl-one".into(), taken_at: now() + Duration::days(60),
            windows: vec![UsageWindow { resets_at: Some(now() + Duration::days(60)), ..window("weekly", 1.0, None) }] }];
        store.put(&file_name(MACHINE_A).unwrap(), &seal(&later, &key).unwrap()).unwrap();
        let mut b = Syncer::default();
        b.sync(&store, &pass("synthetic roster passphrase"), (MACHINE_B, "B"), &[], now() + Duration::minutes(1), false, false);
        assert_eq!(b.seen.peers[0].readings[0].reading.taken_at, now());
        assert_eq!(b.seen.peers[0].readings[0].reading.windows[0].resets_at, None, "two months from the reading, once dated");
        // A roster with fields from a newer SlopBar still opens; one without readings has none.
        let mut future: serde_json::Value = serde_json::to_value(own(MACHINE_A, "A", &[], now())).unwrap();
        future.as_object_mut().unwrap().insert("something_new".into(), serde_json::json!([1, 2]));
        let bytes = crate::backup::seal_bytes(FORMAT, &serde_json::to_vec(&future).unwrap(), &key).unwrap();
        assert!(open(&bytes, &pass("synthetic roster passphrase"), &mut HashMap::new()).unwrap().readings.is_empty());
    }

    #[test]
    fn two_machines_whose_reads_both_failed_do_not_cover_each_other() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        let at = |m: i64| now() + Duration::minutes(m);
        // Both read cl-one, then both monitoring sign-ins ended: each read since has failed for want
        // of a new sign-in.
        let ended = |percent: f64, m: i64| AccountUsage { error: Some("monitor_reconnect".into()), ..reading("claude", "cl-one", percent, Some(m), true) };
        let mut a = Syncer::default();
        let mut b = Syncer::default();
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &[reading("claude", "cl-one", 30.0, Some(0), false)], at(0), false, false);
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[reading("claude", "cl-one", 31.0, Some(2), false)], at(2), false, false);
        let mut on_a = vec![ended(30.0, 0)];
        let mut on_b = vec![ended(31.0, 2)];
        // Their failed reads reach the rosters within a gap, as a change of the reading.
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &on_a, at(READINGS_GAP_MINUTES), false, false);
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &on_b, at(READINGS_GAP_MINUTES + 2), false, false);
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &on_a, at(6 * 60), true, false);
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &on_b, at(6 * 60), true, false);
        apply_readings(&mut on_a, &a.seen, "DESK-A", margin(), at(6 * 60));
        apply_readings(&mut on_b, &b.seen, "LAPTOP-B", margin(), at(6 * 60));
        assert_eq!((on_a[0].covered_by.as_deref(), on_b[0].covered_by.as_deref()), (None, None), "neither takes readings any more");
        // A shows B's last reading, a little newer than its own by more than a refresh; nothing is covered.
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[ended(31.0, 20)], at(6 * 60 + 1), true, false);
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &on_a, at(6 * 60 + 2), true, false);
        apply_readings(&mut on_a, &a.seen, "DESK-A", margin(), at(6 * 60 + 2));
        assert_eq!((shared_percent(&on_a[0]), on_a[0].covered_by.as_deref()), (Some(("LAPTOP-B".into(), 31.0)), None));
        // B signs in again and reads it: it covers A at its next write.
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[reading("claude", "cl-one", 33.0, Some(6 * 60 + 20), false)], at(6 * 60 + 20), true, false);
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &on_a, at(6 * 60 + 21), true, false);
        apply_readings(&mut on_a, &a.seen, "DESK-A", margin(), at(6 * 60 + 21));
        assert_eq!(on_a[0].covered_by.as_deref(), Some("LAPTOP-B"));
        // A read on B that fails for a moment (a rate limit, the network) lapses nothing: A stays covered.
        for error in ["rate_limited", "monitor_network", "http_502"] {
            let blip = AccountUsage { error: Some(error.into()), ..reading("claude", "cl-one", 33.0, Some(6 * 60 + 20), true) };
            b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[blip], at(7 * 60), true, false);
            a.sync(&store, &p, (MACHINE_A, "DESK-A"), &on_a, at(7 * 60 + 1), true, false);
            apply_readings(&mut on_a, &a.seen, "DESK-A", margin(), at(7 * 60 + 1));
            assert_eq!(on_a[0].covered_by.as_deref(), Some("LAPTOP-B"), "{error}");
        }
    }

    #[test]
    fn readings_turned_off_leave_the_roster_at_once_and_a_failing_store_is_tried_once_a_gap() {
        let shared = TempDir::new();
        let store = Counting(Folder(shared.0.clone()), Default::default());
        let p = pass("synthetic roster passphrase");
        let at = |m: i64| now() + Duration::minutes(m);
        let one = [reading("claude", "cl-one", 30.0, Some(0), false)];
        let mut sync = Syncer::default();
        sync.sync(&store, &p, (MACHINE_A, "A"), &one, at(0), false, false);
        assert_eq!(open(&std::fs::read(roster_file(&shared, MACHINE_A)).unwrap(), &p, &mut HashMap::new()).unwrap().readings.len(), 1);
        // Turned off a minute later: rewritten at the next refresh, not after the gap.
        sync.withhold_readings = true;
        sync.sync(&store, &p, (MACHINE_A, "A"), &one, at(1), false, false);
        assert_eq!(store.1.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(open(&std::fs::read(roster_file(&shared, MACHINE_A)).unwrap(), &p, &mut HashMap::new()).unwrap().readings.is_empty());
        sync.sync(&store, &p, (MACHINE_A, "A"), &one, at(2), false, false);
        assert_eq!(store.1.load(std::sync::atomic::Ordering::SeqCst), 2, "once out, nothing more to write");
        // Turned off just after a write of readings alone failed: still at once.
        let mut retried = Syncer::default();
        let dir = TempDir::new();
        retried.sync(&Folder(dir.0.clone()), &p, (MACHINE_A, "A"), &one, at(0), false, false);
        let failing_once = Failing(Default::default());
        retried.sync(&failing_once, &p, (MACHINE_A, "A"), &[reading("claude", "cl-one", 40.0, Some(15), false)], at(READINGS_GAP_MINUTES), false, false);
        assert_eq!(failing_once.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        retried.withhold_readings = true;
        let back = Counting(Folder(dir.0.clone()), Default::default());
        retried.sync(&back, &p, (MACHINE_A, "A"), &one, at(READINGS_GAP_MINUTES + 1), false, false);
        assert_eq!(back.1.load(std::sync::atomic::Ordering::SeqCst), 1, "the store answers again: out at once");
        // A store that fails: the first try at once, then one a gap.
        let mut failing_sync = Syncer::default();
        let dir = TempDir::new();
        failing_sync.sync(&Folder(dir.0.clone()), &p, (MACHINE_A, "A"), &one, at(0), false, false);
        failing_sync.withhold_readings = true;
        let failing = Failing(Default::default());
        for m in 1..=READINGS_GAP_MINUTES + 1 {
            failing_sync.sync(&failing, &p, (MACHINE_A, "A"), &one, at(m), false, false);
        }
        assert_eq!(failing.0.load(std::sync::atomic::Ordering::SeqCst), 2, "at once, then after the gap");
    }

    #[test]
    fn unused_codex_windows_on_a_clock_off_the_providers_cost_no_writes() {
        // This machine's clock runs 20 minutes behind the provider's: every reset reads as a
        // window length and 20 minutes after the read.
        let unused = |m: i64| {
            let mut a = account("codex", "cx-idle");
            let read_at = now() + Duration::minutes(m);
            let provider_now = read_at + Duration::minutes(20);
            a.windows = vec![
                UsageWindow { kind: "session".into(), window_seconds: Some(18_000), used_percent: 0.0, resets_at: Some(provider_now + Duration::seconds(18_000)) },
                UsageWindow { kind: "weekly".into(), window_seconds: Some(604_800), used_percent: 0.0, resets_at: Some(provider_now + Duration::seconds(604_800)) },
            ];
            a.fetched_at = Some(read_at);
            a
        };
        let shared = TempDir::new();
        let store = Counting(Folder(shared.0.clone()), Default::default());
        let p = pass("synthetic roster passphrase");
        let mut sync = Syncer::default();
        for m in (0..24 * 60).step_by(5) {
            sync.sync(&store, &p, (MACHINE_A, "A"), &[unused(m as i64)], now() + Duration::minutes(m as i64), false, false);
        }
        let writes = store.1.load(std::sync::atomic::Ordering::SeqCst);
        assert!(writes <= 5, "{writes} writes for a day of nothing changing");
        // A used window whose reset moved did change: its limit reset and was used again.
        let used = |percent: f64, reset_hours: i64| Reading { lapsed: false, provider: Provider::Codex, id: "cx".into(), taken_at: now(),
            windows: vec![UsageWindow { kind: "session".into(), window_seconds: Some(18_000), used_percent: percent, resets_at: Some(now() + Duration::hours(reset_hours)) }] };
        assert!(!same_readings(&[used(5.0, 1)], &[used(5.0, 5)]));
        // Even when it moved by just the time between the reads: a used window's reset is fixed,
        // so this one is a new window that reached the same percent.
        let later = Reading { taken_at: now() + Duration::hours(5), windows: vec![UsageWindow { resets_at: Some(now() + Duration::hours(6)), ..used(5.0, 1).windows[0].clone() }], ..used(5.0, 1) };
        assert!(!same_readings(&[used(5.0, 1)], &[later]));
        assert!(same_readings(&[used(0.0, 5)], &[Reading { taken_at: now() + Duration::minutes(5), windows: vec![UsageWindow { resets_at: Some(now() + Duration::hours(5) + Duration::minutes(5)), ..used(0.0, 5).windows[0].clone() }], ..used(0.0, 5) }]));
        assert!(!same_readings(&[used(0.0, 5)], &[Reading { lapsed: true, ..used(0.0, 5) }]), "a sign-in that lapsed is a change");
    }

    #[test]
    fn a_machine_named_by_nothing_shares_no_reading_and_ties_go_to_the_newer_roster() {
        let window = || vec![UsageWindow { kind: "weekly".into(), window_seconds: None, used_percent: 5.0, resets_at: None }];
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        let key = SealKey::new(&p).unwrap();
        let mut nameless = own(MACHINE_A, "\u{200B}\u{202E} ", &[account("claude", "cl-one")], now());
        nameless.readings = vec![Reading { lapsed: false, provider: Provider::Claude, id: "cl-one".into(), taken_at: now(), windows: window() }];
        store.put(&file_name(MACHINE_A).unwrap(), &seal(&nameless, &key).unwrap()).unwrap();
        let mut b = Syncer::default();
        b.sync(&store, &p, (MACHINE_B, "B"), &[], now() + Duration::minutes(1), false, false);
        assert_eq!((b.seen.peers[0].machine.as_str(), b.seen.peers[0].readings.len()), ("", 0));
        // Two machines with the same reading time: the one whose roster was written later wins.
        let peer = |id: &str, machine: &str, written: i64| Peer { machine_id: id.into(), machine: machine.into(), written_at: now() + Duration::minutes(written),
            accounts: vec![], run: None, listed_at: now(),
            readings: vec![Received { reading: Reading { lapsed: false, provider: Provider::Claude, id: "cl-one".into(), taken_at: now(), windows: window() }, seen_at: now() }] };
        for order in [[0, 1], [1, 0]] {
            let peers = [peer(MACHINE_A, "EARLIER", 1), peer(MACHINE_B, "LATER", 2)];
            let seen = Seen { peers: order.iter().map(|&i| peers[i].clone()).collect(), ..Seen::default() };
            let mut list = [reading("claude", "cl-one", 1.0, None, true)];
            apply_readings(&mut list, &seen, "HERE", margin(), now() + Duration::minutes(3));
            assert_eq!((shared_percent(&list[0]).map(|s| s.0), list[0].covered_by.as_deref()), (Some("LATER".into()), Some("LATER")));
        }
    }

    /// Where app.rs publishes a snapshot before this refresh's roster sync (the automatic switch's
    /// notice, the early redraw), what other machines' readings show is worked out from the rosters
    /// last read; after the sync, from what it read; and a new passphrase brings a refresh that works
    /// it out from the rosters that passphrase opens.
    #[test]
    fn the_app_works_shared_readings_out_wherever_it_publishes() {
        use crate::source_guard::{function, has, tokens, update};
        const HERE: &str = "roster::tests::the_app_works_shared_readings_out_wherever_it_publishes (src-tauri/src/roster.rs)";
        let app = tokens(include_str!("app.rs"));
        let locked = function(&app, "app.rs", "async fn refresh_locked(", HERE);
        assert!(has(locked, "let mut accounts = list.clone(); carry_over(&mut accounts, &previous); apply_shared(&state, &settings, &mut accounts);             *state.snapshot.lock().unwrap() = Snapshot { accounts, updated_at: Some(chrono::Utc::now()) };"), "the automatic switch's publish. {}", update(HERE));
        assert!(has(locked, "let mut early = list.clone(); carry_over(&mut early, &previous); apply_shared(&state, &settings, &mut early);"),
            "the early redraw. {}", update(HERE));
        assert!(has(locked, "sync_roster(app, &settings, list.clone(), false, false).await; apply_shared(&state, &settings, &mut list);"),
            "after the roster sync. {}", update(HERE));
        let passphrase = function(&app, "app.rs", "async fn set_roster_passphrase(", HERE);
        assert!(has(passphrase, "let status = roster_status(app, true).await; refresh_soon(app); Ok(status)"), "a new passphrase. {}", update(HERE));
    }

    #[test]
    fn rosters_from_others_are_trimmed_to_sane_fields() {
        let mut roster = own(MACHINE_A, "A\u{7}PC", &[account("claude", "cl-one")], now());
        roster.accounts.push(Entry { provider: Provider::Claude, id: "x".repeat(FIELD_MAX + 1), email: None, plan: None });
        roster.accounts.push(Entry { provider: Provider::Codex, id: "cx\nbad".into(), email: None, plan: None });
        let key = SealKey::new(&pass("synthetic roster passphrase")).unwrap();
        let back = open(&seal(&roster, &key).unwrap(), &pass("synthetic roster passphrase"), &mut HashMap::new()).unwrap();
        assert_eq!(back.accounts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["cl-one"]);
        assert_eq!(back.machine, "APC");
    }

    /// A sync service that puts an older copy of this machine's roster back (a restore of an earlier
    /// version), something that writes over it, or a copy that goes: the next read finds it and the
    /// next sync writes the roster again, so the other machines see this machine's readings as they
    /// are. A copy listed but not read (a sync app holding it) is neither gone nor changed.
    #[test]
    fn a_roster_put_back_damaged_or_lost_is_written_again_at_the_next_sync() {
        let p = pass("synthetic roster passphrase");
        let at = |m: i64| now() + Duration::minutes(m);
        let read = |percent: f64, m: i64| [reading("claude", "cl-one", percent, Some(m), false)];
        // An older copy put back.
        let shared = TempDir::new();
        let store = Counting(Folder(shared.0.clone()), Default::default());
        let writes = || store.1.load(std::sync::atomic::Ordering::SeqCst);
        let file = roster_file(&shared, MACHINE_A);
        let mut a = Syncer::default();
        let sync = |a: &mut Syncer, percent: f64, m: i64| a.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(percent, m), at(m), false, false);
        sync(&mut a, 10.0, 0);
        let old = std::fs::read(&file).unwrap();
        sync(&mut a, 60.0, 15);
        assert_eq!(writes(), 2, "a changed reading goes out after the gap");
        // The read at 25 finds it, and the sync after writes the roster again, though nothing else is
        // due (the reading has not changed since 15).
        std::fs::write(&file, &old).unwrap();
        sync(&mut a, 60.0, 20);
        sync(&mut a, 60.0, 25);
        assert_eq!((writes(), a.seen.own_rewritten), (2, 0), "found at the read, written at the next sync");
        sync(&mut a, 60.0, 30);
        assert_eq!((writes(), a.seen.own_rewritten), (3, 1));
        let back = open(&std::fs::read(&file).unwrap(), &p, &mut HashMap::new()).unwrap();
        assert_eq!(back.readings.iter().map(|r| (r.windows[0].used_percent, r.taken_at)).collect::<Vec<_>>(), [(60.0, at(30))]);
        let mut b = Syncer::default();
        let mut here = vec![reading("claude", "cl-one", 1.0, None, true)];
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &here, at(31), false, false);
        apply_readings(&mut here, &b.seen, "LAPTOP-B", margin(), at(31));
        assert_eq!(shared_percent(&here[0]), Some(("DESK-A".into(), 60.0)), "the other machines see the reading as it is");
        // Gone, damaged, or claiming to be another machine's: the same, each the first of its run.
        let fresh = |change: &dyn Fn(&PathBuf)| {
            let shared = TempDir::new();
            let store = Counting(Folder(shared.0.clone()), Default::default());
            let file = roster_file(&shared, MACHINE_A);
            let mut a = Syncer::default();
            a.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(10.0, 0), at(0), false, false);
            change(&file);
            a.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(10.0, 0), at(10), false, false);
            a.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(10.0, 0), at(15), false, false);
            let back = std::fs::read(&file).ok().and_then(|bytes| open(&bytes, &p, &mut HashMap::new()).ok());
            (store.1.load(std::sync::atomic::Ordering::SeqCst), a.seen.own_rewritten, back.map(|r| r.machine_id))
        };
        let rewritten = (2, 1, Some(MACHINE_A.to_string()));
        assert_eq!(fresh(&|file| std::fs::remove_file(file).unwrap()), rewritten, "gone");
        assert_eq!(fresh(&|file| std::fs::write(file, b"{\"format\":\"slopbar-roster\",\"damaged\":true}").unwrap()), rewritten, "damaged");
        let key = SealKey::new(&p).unwrap();
        // `file_name` keeps hex digits and dashes only, so an id with a letter added maps to this
        // machine's file: under its name, a roster saying it is another machine's is nobody's.
        let claimed = seal(&own(&format!("{MACHINE_A}Z"), "DESK-A", &[account("claude", "cl-one")], at(1)), &key).unwrap();
        assert_eq!(fresh(&|file| std::fs::write(file, &claimed).unwrap()), rewritten, "another machine's under this one's name");
        // Listed but not read (a sync app holds it): nothing is written.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let held = std::cell::RefCell::new(None);
            let (writes, rewritten, _) = fresh(&|file| { *held.borrow_mut() = Some(std::fs::OpenOptions::new().read(true).share_mode(0).open(file).unwrap()); });
            assert_eq!((writes, rewritten), (1, 0), "unread: neither gone nor changed");
        }
        // A machine's first sync since it started has written nothing to compare with: it reads its
        // old copy (to take back a failed run) and writes, as every start does.
        let mut restarted = Syncer::default();
        std::fs::write(&file, &old).unwrap();
        restarted.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(60.0, 80), at(80), false, false);
        assert_eq!(restarted.seen.own_rewritten, 0);
    }

    /// While something else keeps writing this machine's file (a copy of this machine with the same
    /// id, or a sync service putting an older copy back again and again), the roster is written
    /// again at once, then 15 minutes, an hour and 6 hours after the time before, then every 6
    /// hours: at the live pace (a read every 15 seconds) too. A day without one starts over. A
    /// store whose writes fail is tried once a gap in all.
    #[test]
    fn a_roster_something_else_keeps_writing_is_written_again_ever_less_often() {
        let shared = TempDir::new();
        let store = Counting(Folder(shared.0.clone()), Default::default());
        let writes = || store.1.load(std::sync::atomic::Ordering::SeqCst);
        let p = pass("synthetic roster passphrase");
        let key = SealKey::new(&p).unwrap();
        let at = |s: i64| now() + Duration::seconds(s);
        let one = [account("claude", "cl-one")];
        let file = roster_file(&shared, MACHINE_A);
        let theirs = seal(&own(MACHINE_A, "DESK-A-COPY", &one, at(0)), &key).unwrap();
        // Another machine's run goes on all along (its roster rewritten every minute), so this
        // machine reads every 15 seconds (`READ_LIVE_SECONDS`).
        let b_file = file_name(MACHINE_B).unwrap();
        let running = |s: i64| {
            let mut r = own(MACHINE_B, "LAPTOP-B", &one, at(s));
            r.run = Some(Run { at: at(s), ..run(1, false, "running") });
            seal(&r, &key).unwrap()
        };
        store.0.put(&b_file, &running(0)).unwrap();
        let mut a = Syncer::default();
        let mut rewrites = vec![];
        for s in (0..=8 * 3600).step_by(READ_LIVE_SECONDS as usize) {
            if s % 60 == 0 {
                store.0.put(&b_file, &running(s)).unwrap();
            }
            // The other writer, before every sync.
            if s > 0 {
                std::fs::write(&file, &theirs).unwrap();
            }
            let before = a.seen.own_rewritten;
            a.sync(&store, &p, (MACHINE_A, "DESK-A"), &one, at(s), false, false);
            if a.seen.own_rewritten > before {
                rewrites.push(s);
            }
        }
        // Found by the read at 15 s: written at 30 s, then after 15 minutes, an hour, 6 hours.
        assert_eq!(rewrites, [30, 30 + 900, 30 + 900 + 3600, 30 + 900 + 3600 + 6 * 3600]);
        assert_eq!(writes(), 1 + rewrites.len(), "nothing but the first write and the rewrites");
        // It stops, and B's run ends: a day later, an older copy put back once is written over at the
        // next sync after the read that finds it, as the first time.
        let mut ended = own(MACHINE_B, "LAPTOP-B", &one, at(8 * 3600));
        ended.run = Some(Run { at: at(8 * 3600), ..run(2, true, "done") });
        store.0.put(&b_file, &seal(&ended, &key).unwrap()).unwrap();
        let calm = 8 * 3600 + 25 * 3600;
        for s in (8 * 3600 + 300..calm).step_by(300) {
            a.sync(&store, &p, (MACHINE_A, "DESK-A"), &one, at(s), false, false);
        }
        std::fs::write(&file, &theirs).unwrap();
        let before = a.seen.own_rewritten;
        for s in (calm..calm + 20 * 60).step_by(300) {
            a.sync(&store, &p, (MACHINE_A, "DESK-A"), &one, at(s), false, false);
        }
        assert_eq!(a.seen.own_rewritten, before + 1, "after a calm day, at once again");
        // A store whose writes fail and whose listing has no file of this machine's (the sharing
        // folder gone, say): one try a gap, as for readings alone.
        let mut b = Syncer::default();
        b.sync(&Folder(shared.0.clone()), &p, (MACHINE_B, "LAPTOP-B"), &one, at(0), false, false);
        let failing = Failing(Default::default());
        for m in 1..=2 * READINGS_GAP_MINUTES {
            b.sync(&failing, &p, (MACHINE_B, "LAPTOP-B"), &one, at(60 * m), false, false);
        }
        assert_eq!(failing.0.load(std::sync::atomic::Ordering::SeqCst), 2, "the read at 10 minutes finds it gone: tried at 11 and 26");
    }

    /// Only the file named after this machine is its roster, as only a file named after a machine
    /// is that machine's: a copy beside it that claims this machine (a sync service's conflicted
    /// copy) is neither counted nor taken back after a restart, in whatever order it is listed.
    #[test]
    fn a_copy_claiming_this_machine_is_not_its_roster() {
        let shared = TempDir::new();
        let store = Folder(shared.0.clone());
        let p = pass("synthetic roster passphrase");
        let key = SealKey::new(&p).unwrap();
        let mut a = Syncer::default();
        a.offer_run(run(4, true, "failed"));
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &[account("claude", "cl-one")], now(), false, false);
        let mut copy = own(MACHINE_A, "DESK-A", &[account("claude", "cl-one"), account("claude", "cl-two"), account("codex", "cx-one")], now());
        copy.run = Some(run(9, true, "failed"));
        for name in [format!("{MACHINE_A} (1).roster"), format!("{MACHINE_A}~2.roster")] {
            store.put(&name, &seal(&copy, &key).unwrap()).unwrap();
        }
        let mut restarted = Syncer::default();
        restarted.sync(&store, &p, (MACHINE_A, "DESK-A"), &[account("claude", "cl-one")], now() + Duration::minutes(1), true, false);
        let mut b = Syncer::default();
        b.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[], now() + Duration::minutes(2), false, false);
        assert_eq!(peer_runs(&b.seen, now() + Duration::minutes(2)).iter().map(|r| r.run.revision).collect::<Vec<_>>(), [4], "the run taken back is its own file's");
        restarted.sync(&store, &p, (MACHINE_A, "DESK-A"), &[account("claude", "cl-one")], now() + Duration::minutes(3), true, false);
        assert_eq!(restarted.seen.own_accounts, Some(1));
        assert_eq!(restarted.seen.own_rewritten, 0, "copies beside it are not its file: nothing to put back");
        // A failed run dated ahead of this clock (a clock that ran fast, or a planted one) is not
        // taken back after a restart: it would count as recent until its date.
        let mut ahead = own(MACHINE_A, "DESK-A", &[account("claude", "cl-one")], now());
        ahead.run = Some(Run { at: now() + Duration::days(30), ..run(7, true, "failed") });
        store.put(&file_name(MACHINE_A).unwrap(), &seal(&ahead, &key).unwrap()).unwrap();
        let mut again = Syncer::default();
        again.sync(&store, &p, (MACHINE_A, "DESK-A"), &[account("claude", "cl-one")], now() + Duration::minutes(4), false, false);
        let mut c = Syncer::default();
        c.sync(&store, &p, (MACHINE_B, "LAPTOP-B"), &[], now() + Duration::minutes(5), false, false);
        assert!(c.seen.peers[0].run.is_none(), "not taken back, so not written on");
    }

    /// A store whose writes land but say they failed, as a write cut off at rclone's time limit
    /// after the upload went through.
    struct Landing(Folder);

    impl RosterStore for Landing {
        fn listing(&self, known: Known) -> std::io::Result<Listed> { self.0.listing(known) }
        fn put(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
            self.0.put(name, bytes)?;
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "synthetic: landed, then timed out"))
        }
        fn remove(&self, name: &str) -> std::io::Result<()> { self.0.remove(name) }
    }

    /// A roster this machine tried to write is its own when it is found there, though the write
    /// said it failed: nothing is put back. And a later read that finds its own copy again (the
    /// other writer stopped, or an older listing caught up) calls off a rewrite not yet made.
    #[test]
    fn a_write_that_said_it_failed_but_landed_is_not_put_back_and_a_later_read_decides() {
        let shared = TempDir::new();
        let p = pass("synthetic roster passphrase");
        let at = |m: i64| now() + Duration::minutes(m);
        let read = |percent: f64, m: i64| [reading("claude", "cl-one", percent, Some(m), false)];
        let store = Counting(Folder(shared.0.clone()), Default::default());
        let writes = || store.1.load(std::sync::atomic::Ordering::SeqCst);
        let mut a = Syncer::default();
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(10.0, 0), at(0), false, false);
        // A changed reading's write lands but says it timed out.
        a.sync(&Landing(Folder(shared.0.clone())), &p, (MACHINE_A, "DESK-A"), &read(60.0, 15), at(15), false, false);
        assert_eq!(a.seen.wrote, Some(false));
        let landed = std::fs::read(roster_file(&shared, MACHINE_A)).unwrap();
        assert_eq!(open(&landed, &p, &mut HashMap::new()).unwrap().readings[0].windows[0].used_percent, 60.0);
        // The next read finds it: one of this machine's, so not put back (the readings write goes
        // again after the gap, as any failed one does).
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(60.0, 20), at(20), false, false);
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(60.0, 25), at(25), false, false);
        a.sync(&store, &p, (MACHINE_A, "DESK-A"), &read(60.0, 30), at(30), false, false);
        assert_eq!((a.seen.own_rewritten, writes()), (0, 2), "nothing put back: the write at 30 is the readings' own retry");
        // Put back once (written over at once), then again: that rewrite waits out 15 minutes, and a
        // read meanwhile (Settings watching) finds this machine's own copy back: none is made.
        let mut b = Syncer::default();
        let dir = TempDir::new();
        let other = Counting(Folder(dir.0.clone()), Default::default());
        let file = roster_file(&dir, MACHINE_A);
        let key = SealKey::new(&p).unwrap();
        let theirs = seal(&own(MACHINE_A, "DESK-A-COPY", &[account("claude", "cl-one")], at(0)), &key).unwrap();
        let sync = |b: &mut Syncer, m: i64, soon: bool| b.sync(&other, &p, (MACHINE_A, "DESK-A"), &read(10.0, 0), at(m), false, soon);
        sync(&mut b, 0, false);
        std::fs::write(&file, &theirs).unwrap();
        sync(&mut b, 10, false);
        sync(&mut b, 11, false);
        assert_eq!(b.seen.own_rewritten, 1);
        let mine = std::fs::read(&file).unwrap();
        std::fs::write(&file, &theirs).unwrap();
        sync(&mut b, 12, true);
        std::fs::write(&file, &mine).unwrap();
        sync(&mut b, 13, true);
        let made = other.1.load(std::sync::atomic::Ordering::SeqCst);
        sync(&mut b, 27, false);
        assert_eq!((b.seen.own_rewritten, other.1.load(std::sync::atomic::Ordering::SeqCst)), (1, made), "the latest read found its own: nothing to put back");
    }
}
