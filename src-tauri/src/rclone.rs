//! Presence records on any cloud storage rclone can reach, with no app registration: the user
//! connects their cloud once, with `rclone config` or with SlopBar's guided Dropbox setup
//! (`sharing_setup`, which runs rclone's own sign-in), and rclone keeps its own config and tokens.
//! SlopBar only runs rclone (no window, a hard timeout per call) on `<remote>:SlopBar/presence`,
//! never passes or reads credentials, and never prints rclone's output. rclone reads only proxy
//! environment variables, so SlopBar sets them to the proxy Windows names for the cloud
//! (`proxy::rclone_env`), and does not run rclone where that proxy cannot be used.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::presence::{Kept, Presence, PresenceStore};

/// No rclone call may take longer than this.
const TIMEOUT: Duration = Duration::from_secs(20);
/// Records live here on the remote.
const DIR: &str = "SlopBar/presence";
/// Presence records are small; anything larger is not one.
const MAX_RECORD: u64 = 256 * 1024;
/// rclone's exit code for "directory not found".
const DIR_NOT_FOUND: i32 = 3;
/// rclone's exit code for "file not found".
const FILE_NOT_FOUND: i32 = 4;

/// What one rclone run produced.
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
}

/// Runs rclone with these arguments (and optional stdin). Tests substitute a fake.
pub trait Runner: Send + Sync {
    fn run(&self, args: &[&str], input: Option<&[u8]>) -> std::io::Result<Ran>;
    /// The kind of the remote its commands reach ("dropbox"…), when it is known.
    fn kind(&self) -> Option<String> {
        None
    }
    /// How many files to ask it for in one `cat` (`Rclone::batch`); one, none together.
    fn batch_most(&self) -> usize {
        BATCH_MOST
    }
    /// What a read's batched calls did, for what the next read asks (`Batching::after`).
    fn batched(&self, _read: Batched) {}
}

/// The real rclone executable, and the remote its commands reach (none for commands that stay on
/// this computer: `version`, `listremotes`).
pub struct Cli {
    exe: PathBuf,
    remote: Option<String>,
    /// The routes rclone's proxy comes from: this user's (`proxy::system`), unless a test gives
    /// its own.
    router: Option<std::sync::Arc<crate::proxy::Router>>,
}

impl Cli {
    /// For commands that reach no cloud.
    pub fn local(exe: PathBuf) -> Cli {
        Cli { exe, remote: None, router: None }
    }

    /// For commands on `remote`: each runs through the proxy Windows would use for its cloud
    /// (`proxy::rclone_env`), whose kind is looked up when the first command runs, on the thread
    /// that runs rclone anyway.
    pub fn for_remote(exe: PathBuf, remote: &str) -> Cli {
        Cli { exe, remote: Some(remote.to_string()), router: None }
    }

    /// `for_remote` with these routes in place of this user's.
    #[cfg(test)]
    pub fn routed(exe: PathBuf, remote: &str, router: std::sync::Arc<crate::proxy::Router>) -> Cli {
        Cli { exe, remote: Some(remote.to_string()), router: Some(router) }
    }
}

/// The hosts rclone reaches for a cloud (its sign-in, API, uploads and downloads), whose proxy it
/// gets, and whether they are the cloud's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cloud {
    pub hosts: &'static [&'static str],
    /// False for a kind SlopBar does not know (S3, WebDAV, a crypt or alias wrapping another
    /// remote, or a remote whose kind could not be read): rclone then runs only where no proxy
    /// script decides, since a script may answer the remote's own host its own way.
    pub known: bool,
}

/// What an unknown kind's route is asked for: no bypass list names it (only `*` would), so a manual
/// proxy and the environment answer for it as for any host they do not bypass, never direct
/// because some well-known host happens to be bypassed.
pub const ANY_HOST: &str = "remote.rclone.slopbar.invalid";

/// The cloud of a remote of this kind.
pub fn cloud(kind: &str) -> Cloud {
    let known = |hosts| Cloud { hosts, known: true };
    match kind {
        "dropbox" => known(&["api.dropboxapi.com", "content.dropboxapi.com"]),
        "drive" => known(&["www.googleapis.com", "oauth2.googleapis.com"]),
        // OneDrive's files come from SharePoint and 1drv.com hosts.
        "onedrive" => known(&["graph.microsoft.com", "login.microsoftonline.com", "onedrive.sharepoint.com", "public.files.1drv.com"]),
        "box" => known(&["api.box.com", "upload.box.com", "dl.boxcloud.com"]),
        _ => Cloud { hosts: &[ANY_HOST], known: false },
    }
}

/// Sets `cmd`'s proxy variables to what rclone needs to reach `cloud` where Windows would send
/// SlopBar's own requests; an error (`proxy::ProxyRefused`) when it must not run at all.
pub fn proxy_env(cmd: &mut std::process::Command, cloud: Cloud) -> std::io::Result<()> {
    proxy_env_with(cmd, cloud, crate::proxy::system())
}

/// `proxy_env` with these routes.
fn proxy_env_with(cmd: &mut std::process::Command, cloud: Cloud, router: &crate::proxy::Router) -> std::io::Result<()> {
    let env = crate::proxy::rclone_env(router, cloud.hosts, cloud.known)
        .map_err(|refusal| std::io::Error::other(crate::proxy::ProxyRefused(refusal)))?;
    apply_env(cmd, env);
    Ok(())
}

/// Sets or removes each variable. Windows' variable names are not case-sensitive, so removing
/// `https_proxy` there would remove the `HTTPS_PROXY` just set; elsewhere the lower-case ones go
/// first, since Go reads them too.
pub fn apply_env(cmd: &mut std::process::Command, env: Vec<(&'static str, Option<String>)>) {
    for (var, value) in env {
        if cfg!(not(windows)) {
            cmd.env_remove(var.to_ascii_lowercase());
        }
        match value {
            Some(value) => cmd.env(var, value),
            None => cmd.env_remove(var),
        };
    }
}

impl Runner for Cli {
    fn run(&self, args: &[&str], input: Option<&[u8]>) -> std::io::Result<Ran> {
        let ran = self.run_once(args, input);
        // A listing, upload or deletion that failed may have gone through the proxy of a cloud the
        // remote no longer is (pointed elsewhere since): its kind is looked up again for the next
        // one. Not for a download, which follows a listing that worked: a download failing on its
        // own (its cloud's file hosts unreachable) waits for the listing's failure, or KIND_KEPT.
        let first = args.first().is_some_and(|verb| matches!(*verb, "lsjson" | "rcat" | "deletefile"));
        if let (Some(remote), true) = (&self.remote, first) {
            if !ran.as_ref().is_ok_and(|r| matches!(r.code, Some(0) | Some(DIR_NOT_FOUND) | Some(FILE_NOT_FOUND))) {
                forget_kind(remote);
            }
        }
        ran
    }

    fn kind(&self) -> Option<String> {
        kind_of(&self.exe, self.remote.as_deref()?)
    }

    /// As this rclone's batched calls went lately (`Batching`).
    fn batch_most(&self) -> usize {
        batching().lock().unwrap_or_else(|e| e.into_inner()).get(&self.exe).copied().unwrap_or_default().at(Instant::now()).most
    }

    fn batched(&self, read: Batched) {
        batching().lock().unwrap_or_else(|e| e.into_inner()).entry(self.exe.clone()).or_default().after(read, Instant::now());
    }
}

/// How each rclone's batched calls went, by its path.
fn batching() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, Batching>> {
    static BATCHING: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, Batching>>> = std::sync::OnceLock::new();
    BATCHING.get_or_init(Default::default)
}

/// How an rclone's batched calls have gone lately (`Rclone::batch`): how many files to ask it for at
/// once, how many reads in a row its calls only refused, and when either last went down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Batching {
    most: usize,
    refusals: u8,
    changed: Option<Instant>,
}

impl Default for Batching {
    fn default() -> Self {
        Batching { most: BATCH_MOST, refusals: 0, changed: None }
    }
}

/// Reads in a row whose batched calls only refused (printed nothing and failed, while the same files
/// one at a time could be read) before none is made: one such read can be the network's moment.
const REFUSALS: u8 = 2;

/// How long `Batching` holds after it last went down before it starts over: a slow link or an
/// rclone that refuses the call is asked again in full an hour later (at most one call's worth of
/// records unread, or two refused calls, an hour), and refusals far apart do not add up.
const BATCHING_KEPT: Duration = Duration::from_secs(60 * 60);

/// What one read's batched calls did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Batched {
    /// One printed files (some, at least).
    pub gave: bool,
    /// One printed nothing and failed, while the same files one at a time could be read.
    pub refused: bool,
    /// One ran out of time.
    pub timed_out: bool,
}

impl Batching {
    /// As it stands at `now`: started over once `BATCHING_KEPT` has passed since it last went down.
    fn at(self, now: Instant) -> Batching {
        match self.changed {
            Some(at) if now.saturating_duration_since(at) >= BATCHING_KEPT => Batching::default(),
            _ => self,
        }
    }

    /// After a read at `now`: a call that ran out of time halves how many the next ask for (one:
    /// none together), so a slow link or a stalling file holds back fewer records and a timed-out
    /// chunk is read in smaller ones; a call that gave anything clears the refusals; the
    /// `REFUSALS`th read in a row that only refused stops batching. For `BATCHING_KEPT` (or until
    /// SlopBar restarts).
    fn after(&mut self, read: Batched, now: Instant) {
        *self = self.at(now);
        if read.timed_out {
            self.most = (self.most / 2).max(1);
            self.changed = Some(now);
        }
        if read.gave {
            self.refusals = 0;
        } else if read.refused {
            self.refusals = self.refusals.saturating_add(1);
            self.changed = Some(now);
            if self.refusals >= REFUSALS {
                self.most = 1;
            }
        }
    }
}

impl Cli {
    fn run_once(&self, args: &[&str], input: Option<&[u8]>) -> std::io::Result<Ran> {
        use std::process::Stdio;
        let mut cmd = crate::sources::command(&self.exe.to_string_lossy());
        // Through the proxy Windows names for the remote's cloud, or not at all.
        if let Some(remote) = &self.remote {
            let router: &crate::proxy::Router = match &self.router {
                Some(router) => router,
                None => crate::proxy::system(),
            };
            proxy_env_with(&mut cmd, cloud(&kind_of(&self.exe, remote).unwrap_or_default()), router)?;
        }
        // Never wait on a prompt for an encrypted config's password, and give up on dead networks
        // well inside the timeout.
        cmd.args(args).args(["--ask-password=false", "--contimeout", "10s", "--timeout", "15s"]);
        cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() });
        // rclone's log can name paths and remotes; it is never kept or shown.
        cmd.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = cmd.spawn()?;
        if let (Some(bytes), Some(mut stdin)) = (input, child.stdin.take()) {
            let bytes = bytes.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            });
        }
        let mut stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
        let reader = std::thread::spawn(move || {
            let mut out = vec![];
            let _ = stdout.read_to_end(&mut out);
            out
        });
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Ran { code: status.code(), stdout: reader.join().unwrap_or_default() });
            }
            if started.elapsed() > TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "rclone timed out"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The rclone to run: the copy SlopBar downloaded and verified (see `sharing_setup`), else where
/// WinGet links it, else the first on PATH (last: folders on PATH can be writable by anyone). All
/// read the same rclone config, so the remotes are the same whichever runs.
pub fn find() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "rclone.exe" } else { "rclone" };
    let own = crate::sharing_setup::own_rclone();
    let winget = dirs::data_local_dir().map(|d| d.join("Microsoft").join("WinGet").join("Links").join(exe));
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(exe)).collect::<Vec<_>>())
        .unwrap_or_default();
    own.into_iter().chain(winget).chain(on_path).find(|p| p.is_file())
}

/// Why an rclone found here does not do for the guided setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unusable {
    /// Windows or security software would not let it start (`sharing_setup::refused_to_start`).
    Refused,
    /// It ran and said it is older than the setup needs.
    Old,
    /// It did not run to a version: it failed, was stopped, or said something else.
    Failed,
}

/// Runs `rclone version` and says whether that rclone does for the guided setup: it runs, and is
/// recent enough (`sharing_setup::recent_enough`).
pub fn check(runner: &dyn Runner) -> Result<(), Unusable> {
    match runner.run(&["version"], None) {
        Err(e) if crate::sharing_setup::refused_to_start(&e) => Err(Unusable::Refused),
        Err(_) => Err(Unusable::Failed),
        Ok(ran) if ran.code != Some(0) => Err(Unusable::Failed),
        Ok(ran) if !crate::sharing_setup::recent_enough(&String::from_utf8_lossy(&ran.stdout)) => {
            if String::from_utf8_lossy(&ran.stdout).trim_start().starts_with("rclone v") { Err(Unusable::Old) } else { Err(Unusable::Failed) }
        }
        Ok(_) => Ok(()),
    }
}

/// A remote the user configured in rclone.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Remote {
    /// The remote's name, without the colon.
    pub name: String,
    /// rclone's backend type ("dropbox", "drive", "onedrive"…).
    pub kind: String,
    /// The cloud's name, as Settings shows it.
    pub provider: String,
}

/// The user's rclone remotes. `listremotes --long` prints only names and types, so no
/// credential is ever read (unlike `config dump`).
pub fn remotes(runner: &dyn Runner) -> Vec<Remote> {
    listed(runner).unwrap_or_default()
}

/// The remotes, or None when rclone could not list them: before creating a remote, an empty list
/// and a failed one must differ, or an existing remote of that name would be overwritten.
pub fn listed(runner: &dyn Runner) -> Option<Vec<Remote>> {
    let ran = runner.run(&["listremotes", "--long"], None).ok()?;
    if ran.code != Some(0) {
        return None;
    }
    let found = String::from_utf8_lossy(&ran.stdout)
        .lines()
        .filter_map(|line| {
            let (name, kind) = line.trim().split_once(':')?;
            let kind = kind.trim().to_string();
            valid_remote(name).then(|| Remote { name: name.into(), provider: provider_name(&kind), kind })
        })
        .collect();
    Some(found)
}

fn provider_name(kind: &str) -> String {
    match kind {
        "dropbox" => "Dropbox",
        "drive" => "Google Drive",
        "onedrive" => "OneDrive",
        "box" => "Box",
        "pcloud" => "pCloud",
        "mega" => "MEGA",
        "s3" => "S3",
        "b2" => "Backblaze B2",
        "webdav" => "WebDAV",
        "sftp" => "SFTP",
        "protondrive" => "Proton Drive",
        "koofr" => "Koofr",
        "yandex" => "Yandex Disk",
        "jottacloud" => "Jottacloud",
        "" => "rclone",
        other => other,
    }
    .to_string()
}

/// A remote name SlopBar will pass to rclone: rclone's own characters, and never one that could
/// read as a flag.
pub fn valid_remote(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.starts_with(' ')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.+@ ".contains(c))
}

/// Presence records on an rclone remote.
pub struct Rclone<R: Runner = Cli> {
    remote: String,
    runner: R,
}

impl Rclone<Cli> {
    /// The store for `remote`, if rclone is installed and the name is safe to pass.
    pub fn open(remote: &str) -> Option<Self> {
        if !valid_remote(remote) {
            return None;
        }
        Some(Rclone { remote: remote.into(), runner: Cli::for_remote(find()?, remote) })
    }
}

/// How long a remote's kind is kept before `listremotes` (a process) is asked again: every command
/// needs it (its proxy follows the cloud), and Settings syncs the rosters every minute while it is
/// open. A remote pointed at another cloud meanwhile shows at its first failed command, which
/// forgets the kind (`forget_kind`).
const KIND_KEPT: Duration = Duration::from_secs(30 * 60);

type Kinds = std::sync::Mutex<std::collections::HashMap<String, (String, Instant)>>;

fn kinds() -> &'static Kinds {
    static KINDS: std::sync::OnceLock<Kinds> = std::sync::OnceLock::new();
    KINDS.get_or_init(Default::default)
}

/// `remote`'s kind ("dropbox"…), from `listremotes --long` (names and kinds only), kept a while.
fn kind_of(exe: &std::path::Path, remote: &str) -> Option<String> {
    kept_kind(remote, Instant::now(), || listed(&Cli::local(exe.to_path_buf()))?.into_iter().find(|r| r.name == remote).map(|r| r.kind))
}

/// `remote`'s kind as kept since `look` last found it, under `KIND_KEPT` before `now`; else `look`'s.
fn kept_kind(remote: &str, now: Instant, look: impl FnOnce() -> Option<String>) -> Option<String> {
    if let Some((kind, at)) = kinds().lock().unwrap_or_else(|e| e.into_inner()).get(remote) {
        if now.saturating_duration_since(*at) < KIND_KEPT {
            return Some(kind.clone());
        }
    }
    // Not holding the lock while `listremotes` runs.
    let kind = look()?;
    kinds().lock().unwrap_or_else(|e| e.into_inner()).insert(remote.to_string(), (kind.clone(), now));
    Some(kind)
}

/// Looks `remote`'s kind up again at its next command.
fn forget_kind(remote: &str) {
    kinds().lock().unwrap_or_else(|e| e.into_inner()).remove(remote);
}

impl<R: Runner> Rclone<R> {
    #[cfg(test)]
    pub fn with(remote: &str, runner: R) -> Self {
        Rclone { remote: remote.into(), runner }
    }

    #[cfg(test)]
    fn path(&self, name: &str) -> String {
        self.path_in(DIR, name)
    }

    fn path_in(&self, dir: &str, name: &str) -> String {
        format!("{}:{dir}/{name}", self.remote)
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> std::io::Result<Ran> {
        let ran = self.runner.run(args, input)?;
        match ran.code {
            Some(0) => Ok(ran),
            code => Err(std::io::Error::other(format!("rclone {} failed ({code:?})", args[0]))),
        }
    }

    /// The files directly in `dir` on the remote named `*.<ext>` and at most `max` bytes, with
    /// their contents and modification times. A missing folder is an empty one.
    pub fn files(&self, dir: &str, ext: &str, max: u64) -> std::io::Result<Vec<(String, Vec<u8>, Option<DateTime<Utc>>)>> {
        Ok(self.files_listed(dir, ext, max, Reading::default(), &|_, _, _| None)?.files.into_iter().map(|f| (f.name, f.bytes, f.modified)).collect())
    }

    /// `files`, with the names listed whose download failed, and, with `reading.hashes` on a remote
    /// whose listing carries a content hash this computer can check at no cost (`HASHED_KINDS`:
    /// Dropbox, Google Drive), each file's hash. A file listed with one goes to `known` first, with
    /// its name and size: the bytes it gives back stand in for that file's `cat`, one rclone process
    /// fewer. A file downloaded keeps its listed hash only when its bytes have it (the file can
    /// change between the listing and the download). Without a hash every file is downloaded. The
    /// file named `reading.except` is left out altogether, neither read nor unread. With
    /// `reading.batch`, files listed with a hash are downloaded together where several are due
    /// (`batch`); the files come back in the listing's order, however they were read.
    pub fn files_listed(&self, dir: &str, ext: &str, max: u64, reading: Reading, known: Known) -> std::io::Result<Listed> {
        let path = format!("{}:{dir}", self.remote);
        // `--hash` makes a remote that cannot list hashes compute them, reading every file (a local
        // or SFTP remote): asked only of the clouds whose listing already holds one that counts.
        let hashes = reading.hashes && self.runner.kind().is_some_and(|kind| HASHED_KINDS.contains(&kind.as_str()));
        let mut args = vec!["lsjson", "--files-only", "--no-mimetype"];
        if hashes {
            args.push("--hash");
        }
        args.push(&path);
        let ran = self.runner.run(&args, None)?;
        match ran.code {
            Some(0) => {}
            // Nobody has written yet.
            Some(DIR_NOT_FOUND) => return Ok(Listed::default()),
            code => return Err(std::io::Error::other(format!("rclone lsjson failed ({code:?})"))),
        }
        let entries: Vec<Entry> = serde_json::from_slice(&ran.stdout).map_err(std::io::Error::other)?;
        let suffix = format!(".{ext}");
        let entries: Vec<Entry> = entries.into_iter()
            .filter(|e| !e.is_dir && e.name.ends_with(&suffix) && (0..=max as i64).contains(&e.size) && !e.name.contains('/') && reading.except != Some(e.name.as_str()))
            .collect();
        let listed_hash = |e: &Entry| hashes.then(|| e.hash()).flatten();
        // Each file's bytes and the hash they have, by its place in the listing: kept, or downloaded.
        let mut got: Vec<Option<(Vec<u8>, Option<String>)>> = entries.iter()
            .map(|e| listed_hash(e).and_then(|h| known(&e.name, e.size as u64, &h).map(|bytes| (bytes, Some(h)))))
            .collect();
        let mut given_up = vec![false; entries.len()];
        // Files a batched call failed to give while printing nothing at all, downloaded alone below.
        let mut after_nothing = vec![false; entries.len()];
        let mut batched = Batched::default();
        let most = if reading.batch { self.runner.batch_most() } else { 1 };
        if most > 1 {
            let due: Vec<usize> = (0..entries.len()).filter(|&i| got[i].is_none() && listed_hash(&entries[i]).is_some() && record_name_shaped(&entries[i].name, ext)).collect();
            for chunk in due.chunks(most).filter(|chunk| chunk.len() > 1) {
                let names: Vec<&str> = chunk.iter().map(|&i| entries[i].name.as_str()).collect();
                match self.batch(dir, &names) {
                    Batch::Printed { stdout, separator, complete } => {
                        batched.gave = true;
                        let mut pieces = split(&stdout, separator.as_bytes());
                        // What follows the last separator: nothing, or a file cut off.
                        pieces.pop();
                        for &i in chunk {
                            let (size, hash) = (entries[i].size, listed_hash(&entries[i]).unwrap_or_default());
                            // A file's bytes are the piece with its listed size and hash, wherever it came.
                            match pieces.iter().find(|p| p.len() as i64 == size && holds(&hash, p)) {
                                Some(piece) => got[i] = Some((piece.to_vec(), Some(hash))),
                                // Not there after a call that went wrong: not read this time (`Recall`
                                // covers a presence record). After one that went right, the file
                                // changed or went since the listing: it is downloaded alone, below.
                                None => given_up[i] = !complete,
                            }
                        }
                    }
                    // One file at a time, below; if that works, the call was refused.
                    Batch::Nothing => {
                        chunk.iter().for_each(|&i| after_nothing[i] = true);
                        break;
                    }
                    // None read this time, and none downloaded alone either (each would wait as long).
                    Batch::Failed { timed_out } => {
                        batched.timed_out |= timed_out;
                        chunk.iter().for_each(|&i| given_up[i] = true);
                    }
                }
            }
        }
        // One `cat` for each file still due.
        for i in 0..entries.len() {
            if got[i].is_some() || given_up[i] {
                continue;
            }
            if let Ok(ran) = self.run(&["cat", &self.path_in(dir, &entries[i].name)], None) {
                let hash = listed_hash(&entries[i]).filter(|h| holds(h, &ran.stdout));
                got[i] = Some((ran.stdout, hash));
            }
        }
        // A batched call that printed nothing and failed, where the same files one at a time could
        // be read: an rclone older than 1.63 (it refuses `--separator`), or the network's moment.
        // Where they could not be read either (the network, or a filter set in rclone's
        // environment, which refuses a `cat` of one file as well), it was not refused.
        batched.refused = (0..entries.len()).any(|i| after_nothing[i] && got[i].is_some());
        if batched != Batched::default() {
            self.runner.batched(batched);
        }
        let mut listed = Listed::default();
        for (e, got) in entries.into_iter().zip(got) {
            match got {
                Some((bytes, hash)) => listed.files.push(RemoteFile { name: e.name, bytes, modified: e.mod_time, hash }),
                None => listed.unread.push(e.name),
            }
        }
        Ok(listed)
    }

    /// Downloads `names` from `dir` in one rclone process: `cat --files-from-raw -` with the names
    /// on its input, which prints each file there followed by `--separator` (rclone 1.63 and later;
    /// an older one refuses the flag). The separator is random for each call, but a file can hold
    /// anything, so a piece of the output is only taken for the file whose listed size and hash it
    /// has (`files_listed`). A file gone since the listing is left out of the output, and one that
    /// cannot be read is too, with the call then failing. The files come in rclone's order (by
    /// name on Dropbox, checked with rclone 1.74.4), not the names'. No other filter goes with
    /// `--files-from-raw`: rclone refuses the call then (`--max-size`, checked).
    fn batch(&self, dir: &str, names: &[&str]) -> Batch {
        let separator = uuid::Uuid::new_v4().simple().to_string();
        let input: String = names.iter().map(|name| format!("{name}\n")).collect();
        let path = format!("{}:{dir}", self.remote);
        match self.runner.run(&["cat", "--files-from-raw", "-", "--separator", &separator, &path], Some(input.as_bytes())) {
            Err(e) => Batch::Failed { timed_out: e.kind() == std::io::ErrorKind::TimedOut },
            Ok(ran) if ran.code != Some(0) && ran.stdout.is_empty() => Batch::Nothing,
            Ok(ran) => Batch::Printed { stdout: ran.stdout, separator, complete: ran.code == Some(0) },
        }
    }

    /// Uploads straight over `<dir>/<name>`, which rclone replaces in one step: a cloud storage
    /// swaps in the finished upload, and a local, SFTP or FTP remote gets a partial file renamed
    /// into place (rclone v1.63 and later). A reader finds the old file or the new one. Uploading
    /// under a temporary name and moving it over the old file left a moment with no file at all,
    /// because rclone deletes an existing file before it moves another onto its name (about a
    /// second on Dropbox), and a reader then skipped this machine for a refresh.
    pub fn put(&self, dir: &str, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        self.run(&["rcat", &self.path_in(dir, name)], Some(bytes)).map(|_| ())
    }

    /// Deletes `<dir>/<name>`; one already gone is fine.
    pub fn delete(&self, dir: &str, name: &str) -> std::io::Result<()> {
        let ran = self.runner.run(&["deletefile", &self.path_in(dir, name)], None)?;
        match ran.code {
            Some(0) | Some(DIR_NOT_FOUND) | Some(FILE_NOT_FOUND) => Ok(()),
            code => Err(std::io::Error::other(format!("rclone deletefile failed ({code:?})"))),
        }
    }
}

/// Given a file's name, size and content hash as listed, the bytes the caller kept from a download
/// of it with that hash, if any (`Rclone::files_listed`).
pub type Known<'a> = &'a dyn Fn(&str, u64, &str) -> Option<Vec<u8>>;

/// How `Rclone::files_listed` reads a folder.
#[derive(Debug, Clone, Copy, Default)]
pub struct Reading<'a> {
    /// List each file's content hash where the cloud lists one this computer checks at no cost.
    pub hashes: bool,
    /// A file left out altogether, neither read nor unread.
    pub except: Option<&'a str>,
    /// Download the files due in one rclone process each `BATCH_MOST`, where each has a hash to
    /// tell its bytes by and a name SlopBar writes (`Rclone::batch`).
    pub batch: bool,
}

/// At most this many files in one batched download at first (fewer after one ran out of time:
/// `Batching`): rclone downloads them one after another, all within one call's `TIMEOUT`.
const BATCH_MOST: usize = 16;

/// How a batched download went (`Rclone::batch`).
enum Batch {
    /// What it printed, the separator each file is followed by, and whether it ended well.
    Printed { stdout: Vec<u8>, separator: String, complete: bool },
    /// It failed having printed nothing: an rclone that does not take the command (older than
    /// 1.63), or no file it could read (the network; a filter set in rclone's environment).
    Nothing,
    /// It did not end (ran out of time) or did not start.
    Failed { timed_out: bool },
}

/// `bytes` cut at each `separator` (the last piece is what follows the last one).
fn split<'a>(bytes: &'a [u8], separator: &[u8]) -> Vec<&'a [u8]> {
    let mut pieces = Vec::new();
    let mut rest = bytes;
    while let Some(at) = (!separator.is_empty()).then(|| rest.windows(separator.len()).position(|w| w == separator)).flatten() {
        pieces.push(&rest[..at]);
        rest = &rest[at + separator.len()..];
    }
    pieces.push(rest);
    pieces
}

/// A name as SlopBar writes them: a machine id's hex digits and dashes, then `.<ext>`. Only such
/// names go on a batched download's input, one per line (a cloud's names can hold line breaks).
fn record_name_shaped(name: &str, ext: &str) -> bool {
    name.strip_suffix(&format!(".{ext}")).is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
}

/// One file as `Rclone::files_listed` found it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub modified: Option<DateTime<Utc>>,
    /// Its keyed content hashes as the listing gave them (`kind:value`, comma-separated), when they
    /// were asked for and the bytes have them.
    pub hash: Option<String>,
}

/// What `Rclone::files_listed` found: the files it has the bytes of, and those listed whose
/// download failed.
#[derive(Debug, Default)]
pub struct Listed {
    pub files: Vec<RemoteFile>,
    pub unread: Vec<String>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Size", default)]
    size: i64,
    #[serde(rename = "ModTime")]
    mod_time: Option<DateTime<Utc>>,
    #[serde(rename = "IsDir", default)]
    is_dir: bool,
    /// With `--hash`: each hash the remote lists, by kind ("dropbox", "md5"…).
    #[serde(rename = "Hashes", default)]
    hashes: std::collections::BTreeMap<String, String>,
}

/// The remotes asked for hashes (`--hash`): clouds whose listing carries one of `KEYED_HASHES` at
/// no cost. OneDrive lists only a quickxor and Box only a SHA-1, so they download every file.
const HASHED_KINDS: [&str; 2] = ["dropbox", "drive"];

/// The hash kinds a file is known by: ones nobody can match with other bytes (unlike OneDrive's
/// quickxor, which whoever can write the folder could), and that this computer checks against the
/// bytes it downloads (`holds`), so a hash listed before the file changed never stands for bytes
/// read after.
const KEYED_HASHES: [&str; 3] = ["dropbox", "sha256", "sha512"];

impl Entry {
    /// Its keyed hashes as one string, in kind order; none when the listing gave none.
    fn hash(&self) -> Option<String> {
        let all: Vec<String> = self.hashes.iter().filter(|(k, v)| KEYED_HASHES.contains(&k.as_str()) && !v.is_empty()).map(|(k, v)| format!("{k}:{v}")).collect();
        (!all.is_empty()).then(|| all.join(","))
    }
}

/// Whether `bytes` have every hash in `hash` (as `Entry::hash` writes them).
fn holds(hash: &str, bytes: &[u8]) -> bool {
    use sha2::{Digest, Sha256, Sha512};
    let hex = |digest: &[u8]| digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    hash.split(',').all(|pair| match pair.split_once(':') {
        Some(("dropbox", v)) => v.eq_ignore_ascii_case(&hex(&dropbox_hash(bytes))),
        Some(("sha256", v)) => v.eq_ignore_ascii_case(&hex(&Sha256::digest(bytes))),
        Some(("sha512", v)) => v.eq_ignore_ascii_case(&hex(&Sha512::digest(bytes))),
        _ => false,
    })
}

/// Dropbox's content hash: the SHA-256 of the SHA-256s of each 4 MiB block, one after another.
fn dropbox_hash(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut blocks = Sha256::new();
    for block in bytes.chunks(4 * 1024 * 1024) {
        blocks.update(Sha256::digest(block));
    }
    blocks.finalize().into()
}

/// The file a machine's presence record goes to: its id's hex digits and dashes, then `.json`.
fn record_name(machine_id: &str) -> Option<String> {
    let safe: String = machine_id.chars().filter(|c| c.is_ascii_hexdigit() || *c == '-').collect();
    (!safe.is_empty()).then(|| format!("{safe}.json"))
}

impl<R: Runner> PresenceStore for Rclone<R> {
    fn list(&self) -> std::io::Result<Vec<(Presence, Option<DateTime<Utc>>)>> {
        Ok(self.files(DIR, "json", MAX_RECORD)?
            .into_iter()
            .filter_map(|(_, bytes, at)| Some((serde_json::from_slice::<Presence>(&bytes).ok()?, at)))
            .collect())
    }

    /// The listing and the records the refresh needs: none for this machine's own file, and on
    /// Dropbox and Google Drive none for a record listed with the content hash (and size) of the
    /// copy `kept` holds, whose bytes stand in for it, and the rest in one `cat` for up to 16 of
    /// them (`Rclone::batch`). `kept` then holds the records this read has the bytes of under a
    /// hash they have; a listing that fails leaves it as it was.
    fn others(&self, own_id: &str, kept: &Kept) -> std::io::Result<Vec<(Presence, Option<DateTime<Utc>>)>> {
        let own = record_name(own_id);
        let reading = Reading { hashes: true, except: own.as_deref(), batch: true };
        let listed = self.files_listed(DIR, "json", MAX_RECORD, reading, &|name, size, hash| kept.get(name, size, hash))?;
        let mut records = Vec::new();
        let mut keep = Vec::new();
        for RemoteFile { name, bytes, modified, hash } in listed.files {
            // Only records: anything else is downloaded again at each read, as it always was.
            let Ok(presence) = serde_json::from_slice::<Presence>(&bytes) else { continue };
            records.push((presence, modified));
            if let Some(hash) = hash {
                keep.push((name, hash, bytes));
            }
        }
        kept.replace(keep);
        Ok(records)
    }

    fn write(&self, presence: &Presence) -> std::io::Result<()> {
        let name = record_name(&presence.machine_id).ok_or_else(|| std::io::Error::other("no machine id"))?;
        self.put(DIR, &name, &serde_json::to_vec_pretty(presence)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::{own, read};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A fake rclone: an in-memory remote that understands the commands SlopBar uses.
    #[derive(Default)]
    struct Fake {
        files: Mutex<BTreeMap<String, Vec<u8>>>,
        /// The remote's kind (`Runner::kind`): with `--hash`, Dropbox lists its block hash and Drive
        /// MD5, SHA-1 and SHA-256, as theirs do, except for the files in `hashless`, and for those in
        /// `stale` the hash of other bytes (the file changed between the listing and the download).
        kind: Option<String>,
        hashless: Mutex<Vec<String>>,
        stale: Mutex<Vec<String>>,
        /// Each upload's modification time, in seconds after a fixed time: every upload takes the
        /// next second of `clock` (set it back for two uploads in one second). A file put in `files`
        /// by hand lists at the fixed time.
        times: Mutex<BTreeMap<String, i64>>,
        clock: std::sync::atomic::AtomicI64,
        /// Files whose `cat` fails, as a download that broke off would.
        unreadable: Mutex<Vec<String>>,
        calls: Mutex<Vec<String>>,
        remotes: String,
        fail: Option<i32>,
        /// A batched `cat` (`--files-from-raw -`): the names each one was given; from its
        /// `refuse_from`th call on, `refuse` fails it with that exit code and nothing printed, as an
        /// rclone before 1.63 does (1; an unknown flag exits 2 from rclone 1.69 on); `hang` times it
        /// out; the files in `guessed` print the call's separator inside their bytes, and those in
        /// `batch_skips` are left out of it (exit 0) though a `cat` of one finds it. It prints the
        /// files in reverse order of their names (rclone's is by name), so nothing goes by order.
        batched: Mutex<Vec<Vec<String>>>,
        refuse: Option<i32>,
        batch_skips: Mutex<Vec<String>>,
        refuse_from: usize,
        batch_calls: std::sync::atomic::AtomicUsize,
        hang: std::sync::atomic::AtomicBool,
        guessed: Mutex<Vec<String>>,
        /// Files listed but gone by the time they are downloaded: a batched call leaves them out, a
        /// `cat` of one finds nothing (exit 3, as of any missing file).
        gone: Mutex<Vec<String>>,
        batching: Mutex<Batching>,
    }

    fn dropbox() -> Fake {
        Fake { kind: Some("dropbox".into()), ..Fake::default() }
    }

    impl Runner for Fake {
        fn run(&self, args: &[&str], input: Option<&[u8]>) -> std::io::Result<Ran> {
            self.calls.lock().unwrap().push(args.join(" "));
            let ok = |stdout: Vec<u8>| Ok(Ran { code: Some(0), stdout });
            if let Some(code) = self.fail {
                return Ok(Ran { code: Some(code), stdout: vec![] });
            }
            let mut files = self.files.lock().unwrap();
            match args {
                ["listremotes", "--long"] => ok(self.remotes.clone().into_bytes()),
                ["lsjson", "--files-only", "--no-mimetype", rest @ ..] => {
                    let (hashes, dir) = match rest {
                        ["--hash", dir] => (true, dir),
                        [dir] => (false, dir),
                        other => panic!("unexpected lsjson: {other:?}"),
                    };
                    let prefix = format!("{dir}/");
                    let (times, hashless) = (self.times.lock().unwrap(), self.hashless.lock().unwrap());
                    let base: DateTime<Utc> = "2026-10-21T07:00:00Z".parse().unwrap();
                    let list: Vec<_> = files
                        .iter()
                        .filter_map(|(k, v)| k.strip_prefix(&prefix).map(|n| {
                            let at = base + chrono::Duration::seconds(times.get(k).copied().unwrap_or(0));
                            let mut entry = serde_json::json!({ "Path": n, "Name": n, "Size": v.len(), "ModTime": at, "IsDir": false });
                            if hashes {
                                use sha2::{Digest, Sha256};
                                let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
                                let bytes: &[u8] = if self.stale.lock().unwrap().contains(k) { b"other bytes" } else { v };
                                entry["Hashes"] = match (hashless.contains(k), self.kind.as_deref()) {
                                    (true, _) => serde_json::json!({}),
                                    (_, Some("drive")) => serde_json::json!({ "md5": "synthetic-md5", "sha1": "synthetic-sha1", "sha256": hex(&Sha256::digest(bytes)) }),
                                    // SHA-256 of each 4 MiB block's SHA-256: of nothing for an empty file.
                                    _ => serde_json::json!({ "dropbox": hex(&Sha256::digest(bytes.chunks(4 * 1024 * 1024).flat_map(|b| Sha256::digest(b).to_vec()).collect::<Vec<u8>>())) }),
                                };
                            }
                            entry
                        }))
                        .collect();
                    if list.is_empty() {
                        return Ok(Ran { code: Some(DIR_NOT_FOUND), stdout: vec![] });
                    }
                    ok(serde_json::to_vec(&list).unwrap())
                }
                // Each named file there, in the folder's order, then the separator; a name gone is left
                // out, an unreadable file too and the call ends failing.
                ["cat", "--files-from-raw", "-", "--separator", separator, dir] => {
                    let call = self.batch_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if let Some(code) = self.refuse.filter(|_| call >= self.refuse_from) {
                        return Ok(Ran { code: Some(code), stdout: vec![] });
                    }
                    if self.hang.load(std::sync::atomic::Ordering::SeqCst) {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "rclone timed out"));
                    }
                    let mut names: Vec<String> = String::from_utf8_lossy(input.unwrap_or_default()).lines().filter(|l| !l.is_empty()).map(String::from).collect();
                    names.sort();
                    self.batched.lock().unwrap().push(names.clone());
                    names.reverse();
                    let (mut out, mut code) = (Vec::new(), 0);
                    for name in names {
                        let path = format!("{dir}/{name}");
                        if self.unreadable.lock().unwrap().contains(&path) {
                            code = 1;
                            continue;
                        }
                        let left_out = self.gone.lock().unwrap().contains(&path) || self.batch_skips.lock().unwrap().contains(&path);
                        let Some(bytes) = files.get(&path).filter(|_| !left_out) else { continue };
                        if self.guessed.lock().unwrap().contains(&path) {
                            let half = bytes.len() / 2;
                            out.extend_from_slice(&bytes[..half]);
                            out.extend_from_slice(separator.as_bytes());
                            out.extend_from_slice(&bytes[half..]);
                        } else {
                            out.extend_from_slice(bytes);
                        }
                        out.extend_from_slice(separator.as_bytes());
                    }
                    Ok(Ran { code: Some(code), stdout: out })
                }
                ["cat", path] if self.unreadable.lock().unwrap().iter().any(|p| p == path) => Ok(Ran { code: Some(1), stdout: vec![] }),
                ["cat", path] => match files.get(*path).filter(|_| !self.gone.lock().unwrap().iter().any(|p| p == path)) {
                    Some(bytes) => ok(bytes.clone()),
                    None => Ok(Ran { code: Some(DIR_NOT_FOUND), stdout: vec![] }),
                },
                ["deletefile", path] => {
                    files.remove(*path);
                    ok(vec![])
                }
                ["rcat", path] => {
                    files.insert(path.to_string(), input.unwrap_or_default().to_vec());
                    let at = self.clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    self.times.lock().unwrap().insert(path.to_string(), at);
                    ok(vec![])
                }
                other => panic!("unexpected rclone call: {other:?}"),
            }
        }

        fn kind(&self) -> Option<String> {
            self.kind.clone()
        }

        fn batch_most(&self) -> usize {
            self.batching.lock().unwrap().at(Instant::now()).most
        }

        fn batched(&self, read: Batched) {
            self.batching.lock().unwrap().after(read, Instant::now());
        }
    }

    impl Fake {
        /// How many of each command ran since the call count was `from`.
        fn ran(&self, from: usize, command: &str) -> usize {
            self.calls.lock().unwrap()[from..].iter().filter(|c| c.split(' ').next() == Some(command)).count()
        }

        fn count(&self) -> usize {
            self.calls.lock().unwrap().len()
        }
    }

    fn account(id: &str) -> crate::model::AccountUsage {
        let mut a: crate::model::AccountUsage = serde_json::from_value(serde_json::json!({
            "id": id, "provider": "claude", "email": format!("{id}@example.invalid"), "alias": null, "plan": "max",
            "org": "Synthetic Org", "active": true, "sources": ["Windows"], "windows": [], "credits": null, "resets": [],
            "error": null, "stale": false, "fetched_at": null, "monitor_connected": false })).unwrap();
        a.active = true;
        a
    }

    #[test]
    fn writes_in_one_step_and_reads_back_through_the_remote() {
        let store = Rclone::with("cloud", Fake::default());
        let now = Utc::now();
        assert!(store.list().unwrap().is_empty(), "a missing folder is an empty one");
        let mine = own("11111111-1111-4111-8111-111111111111", "DESK-1", 300, &[account("synthetic-a:org")], now);
        let other = own("22222222-2222-4222-8222-222222222222", "LAPTOP-2", 300, &[account("synthetic-a:org")], now);
        store.write(&mine).unwrap();
        store.write(&other).unwrap();
        store.write(&other).unwrap();
        let calls = store.runner.calls.lock().unwrap().clone();
        // Replacing a record is one upload over it: a `moveto` onto an existing file deletes that
        // file first, and a reader listing in between would miss this machine.
        assert_eq!(calls.iter().filter(|c| *c == "rcat cloud:SlopBar/presence/22222222-2222-4222-8222-222222222222.json").count(), 2, "{calls:?}");
        let writes: Vec<_> = calls.iter().filter(|c| !c.starts_with("lsjson ")).collect();
        assert!(writes.len() == 3 && writes.iter().all(|c| c.starts_with("rcat ") && c.ends_with(".json")), "nothing but uploads to the records themselves: {calls:?}");
        assert!(calls.iter().all(|c| c.contains("cloud:SlopBar/presence") || c.starts_with("listremotes")), "only the presence folder: {calls:?}");
        for bytes in store.runner.files.lock().unwrap().values() {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains('@') && !text.contains("synthetic-a") && !text.contains("Synthetic Org"), "{text}");
        }
        let peers = read(&store, &mine.machine_id, now);
        assert_eq!(peers.iter().map(|p| p.machine.as_str()).collect::<Vec<_>>(), ["LAPTOP-2"]);
        assert_eq!(peers[0].seats.len(), 1);
        store.clear(&other.machine_id, "LAPTOP-2", 300).unwrap();
        assert!(read(&store, &mine.machine_id, Utc::now())[0].seats.is_empty(), "a clean quit empties the record");
    }

    #[test]
    fn rosters_go_encrypted_to_their_own_folder_on_the_remote() {
        use crate::roster::{RosterStore, Syncer};
        let store = Rclone::with("cloud", Fake::default());
        let passphrase = crate::backup::Passphrase("synthetic roster passphrase".into());
        let mut sync = Syncer::default();
        sync.sync(&store, &passphrase, ("11111111-1111-4111-8111-111111111111", "DESK-1"), &[account("synthetic-a:org")], Utc::now(), false, false);
        assert_eq!((sync.seen.wrote, sync.seen.own_accounts), (Some(true), Some(1)), "written and read back");
        let files = store.runner.files.lock().unwrap().clone();
        assert_eq!(files.keys().collect::<Vec<_>>(), ["cloud:SlopBar/roster/11111111-1111-4111-8111-111111111111.roster"]);
        let text = String::from_utf8_lossy(files.values().next().unwrap());
        assert!(!text.contains('@') && !text.contains("synthetic-a") && !text.contains("DESK-1"), "{text}");
        store.remove("11111111-1111-4111-8111-111111111111.roster").unwrap();
        assert!(RosterStore::list(&store).unwrap().is_empty());
    }

    const ME: &str = "11111111-1111-4111-8111-111111111111";
    const BUSY: &str = "22222222-2222-4222-8222-222222222222";
    const IDLE: &str = "33333333-3333-4333-8333-333333333333";

    /// An account read at `at` with its weekly window at `percent`.
    fn read_at(id: &str, percent: f64, at: DateTime<Utc>) -> crate::model::AccountUsage {
        let mut a = account(id);
        a.windows = vec![crate::model::UsageWindow { kind: "weekly".into(), window_seconds: None, used_percent: percent, resets_at: None }];
        a.fetched_at = Some(at);
        a
    }

    /// Another machine writing its roster, with one reading, as its own sync would.
    fn peer_writes(store: &Rclone<Fake>, key: &crate::backup::SealKey, id: &str, machine: &str, percent: f64, at: DateTime<Utc>, run: Option<crate::roster::Run>) {
        let accounts = [read_at("synthetic-b:org", percent, at)];
        let mut roster = crate::roster::own(id, machine, &accounts, at);
        roster.readings = crate::roster::readings(&accounts, at);
        roster.run = run;
        crate::roster::RosterStore::put(store, &crate::roster::file_name(id).unwrap(), &crate::roster::seal(&roster, key).unwrap()).unwrap();
    }

    /// A roster listed with the content hash of the bytes the last read kept of it is not
    /// downloaded again, at any pace: those bytes are its bytes, so its machine and readings stay as
    /// they are and are listed now. A file that changed, in any way, is downloaded; so is every file
    /// at a forced read (sync_roster), and any file listed with no hash.
    #[test]
    fn roster_reads_download_only_files_whose_content_changed() {
        use crate::roster::Syncer;
        let store = Rclone::with("cloud", dropbox());
        let passphrase = crate::backup::Passphrase("synthetic roster passphrase".into());
        let key = crate::backup::SealKey::new(&passphrase).unwrap();
        let t0: DateTime<Utc> = "2026-11-22T12:00:00Z".parse().unwrap();
        let at = |s: i64| t0 + chrono::Duration::seconds(s);
        let min = |m: i64| 60 * m;
        peer_writes(&store, &key, BUSY, "BUSY-PC", 10.0, at(0), None);
        peer_writes(&store, &key, IDLE, "IDLE-PC", 20.0, at(0), None);
        let mine = [account("synthetic-a:org")];
        let mut me = Syncer::default();
        let cats = |me: &mut Syncer, s: i64, force: bool, soon: bool| {
            let from = store.runner.count();
            me.sync(&store, &passphrase, (ME, "DESK-1"), &mine, at(s), force, soon);
            assert!(store.runner.calls.lock().unwrap()[from..].iter().filter(|c| c.starts_with("lsjson ")).all(|c| c.contains(" --hash ")), "a cloud that lists hashes is asked for them");
            store.runner.ran(from, "cat")
        };
        assert_eq!(cats(&mut me, 0, false, false), 2, "the first read since SlopBar started downloads both, before its first write");
        assert_eq!(cats(&mut me, min(10), false, false), 1, "its own roster, new since");
        assert_eq!(cats(&mut me, min(20), false, false), 0, "nothing changed: the listing alone");
        let shown = |me: &Syncer| me.seen.peers.iter().map(|p| (p.machine.clone(), p.readings.len(), p.listed_at)).collect::<Vec<_>>();
        assert_eq!(shown(&me), [("BUSY-PC".to_string(), 1, at(min(20))), ("IDLE-PC".to_string(), 1, at(min(20)))], "kept as read, and listed now");
        peer_writes(&store, &key, BUSY, "BUSY-PC", 30.0, at(min(25)), None);
        assert_eq!(cats(&mut me, min(30), false, false), 1, "only the one that changed");
        let percent = |me: &Syncer, machine: &str| me.seen.peers.iter().find(|p| p.machine == machine).unwrap().readings[0].reading.windows[0].used_percent;
        assert_eq!(percent(&me, "BUSY-PC"), 30.0);
        // Settings watching reads every 30 seconds, and downloads only what changed; a forced read
        // downloads everything.
        assert_eq!(cats(&mut me, min(31), false, true), 0);
        assert_eq!(cats(&mut me, min(32), true, false), 3);
        // A run going on elsewhere: the read that finds it, then the live pace, which downloads only
        // what changed too.
        let run = |at: DateTime<Utc>| Some(crate::roster::Run { revision: 1, at, total: 1, done: 0, emails: 1, email: 1,
            finished: false, still_waiting: false, monitoring: true, everywhere: false, error: None, steps: vec![], started_at: None, open: None });
        peer_writes(&store, &key, BUSY, "BUSY-PC", 30.0, at(min(33)), run(at(min(33))));
        assert_eq!(cats(&mut me, min(42), false, false), 1);
        assert_eq!(cats(&mut me, min(42) + 15, false, false), 0, "read at the live pace: nothing changed");
        assert_eq!(me.seen.read_at, Some(at(min(42) + 15)));
        peer_writes(&store, &key, BUSY, "BUSY-PC", 31.0, at(min(43)), run(at(min(43))));
        assert_eq!(cats(&mut me, min(43), false, false), 1);
        // Its machine goes quiet mid-run (stopped, or gone for good): after QUIET_MINUTES it holds
        // no other machine at the live pace any more.
        assert_eq!(cats(&mut me, min(43) + 15, false, false), 0);
        cats(&mut me, min(64), false, false);
        assert_eq!(me.seen.read_at, Some(at(min(64))));
        cats(&mut me, min(64) + 15, false, false);
        cats(&mut me, min(70), false, false);
        assert_eq!(me.seen.read_at, Some(at(min(64))), "quiet for 21 minutes: the slow pace");
        // A rewrite in the same second at the same size is still a change: its hash is another.
        let idle_path = format!("cloud:SlopBar/roster/{IDLE}.roster");
        let before = store.runner.times.lock().unwrap()[&idle_path];
        let size = store.runner.files.lock().unwrap()[&idle_path].len();
        store.runner.clock.store(before - 1, std::sync::atomic::Ordering::SeqCst);
        peer_writes(&store, &key, IDLE, "IDLE-PC", 25.0, at(min(71)), None);
        assert_eq!((store.runner.times.lock().unwrap()[&idle_path], store.runner.files.lock().unwrap()[&idle_path].len()), (before, size));
        assert_eq!((cats(&mut me, min(74), false, false), percent(&me, "IDLE-PC")), (1, 25.0));
        // A file listed with no hash is downloaded at every read.
        store.runner.hashless.lock().unwrap().push(idle_path.clone());
        assert_eq!(cats(&mut me, min(84), false, false), 1);
        assert_eq!(cats(&mut me, min(94), false, false), 1);
        store.runner.hashless.lock().unwrap().clear();
        assert_eq!(cats(&mut me, min(104), false, false), 1, "listed with a hash again: read once more (nothing was kept of it without one), then known");
        assert_eq!(cats(&mut me, min(114), false, false), 0);
        // A file whose listed hash its downloaded bytes do not have (it changed between the listing
        // and the download) is kept under no hash: downloaded again at the next read.
        store.runner.stale.lock().unwrap().push(idle_path.clone());
        peer_writes(&store, &key, IDLE, "IDLE-PC", 26.0, at(min(115)), None);
        assert_eq!(cats(&mut me, min(124), false, false), 1);
        assert_eq!(cats(&mut me, min(134), false, false), 1, "its bytes never stood for that hash");
        store.runner.stale.lock().unwrap().clear();
        assert_eq!(cats(&mut me, min(144), false, false), 1);
        assert_eq!(cats(&mut me, min(154), false, false), 0);
        // Google Drive lists MD5, SHA-1 and SHA-256: known by its SHA-256. Remotes whose listing
        // carries none this computer counts are not asked for hashes and download every file: those
        // with none at all (S3, WebDAV, SFTP, local: rclone would compute them by reading every
        // file), OneDrive (a quickxor, which other bytes can be made to match) and Box (a SHA-1).
        for (kind, asked, downloads) in [("drive", true, 1 + 1), ("sftp", false, 1 + 2 + 2), ("onedrive", false, 1 + 2 + 2), ("box", false, 1 + 2 + 2)] {
            let plain = Rclone::with("cloud", Fake { kind: Some(kind.into()), ..Fake::default() });
            peer_writes(&plain, &key, IDLE, "IDLE-PC", 20.0, at(0), None);
            let mut other = Syncer::default();
            for m in [0, 10, 20] {
                other.sync(&plain, &passphrase, (ME, "DESK-1"), &mine, at(min(m)), false, false);
            }
            let calls = plain.runner.calls.lock().unwrap().clone();
            assert!(calls.iter().filter(|c| c.starts_with("lsjson ")).all(|c| c.contains("--hash") == asked), "{kind}: {calls:?}");
            assert_eq!(calls.iter().filter(|c| c.starts_with("cat ")).count(), downloads, "{kind}: {calls:?}");
        }
    }

    /// The hashes the clouds list, checked against the bytes as they compute them: Dropbox's is the
    /// SHA-256 of each 4 MiB block's SHA-256 (checked on a real roster: a file under 4 MiB lists
    /// SHA-256(SHA-256(bytes))).
    #[test]
    fn listed_hashes_are_checked_against_the_bytes() {
        use sha2::{Digest, Sha256};
        let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let small = b"synthetic roster bytes".to_vec();
        assert!(holds(&format!("dropbox:{}", hex(&Sha256::digest(Sha256::digest(&small)))), &small));
        assert!(holds(&format!("dropbox:{}", hex(&Sha256::digest(Sha256::digest(&small))).to_uppercase()), &small), "hex in either case");
        let big = vec![7u8; 4 * 1024 * 1024 + 3];
        let blocks: Vec<u8> = big.chunks(4 * 1024 * 1024).flat_map(|b| Sha256::digest(b).to_vec()).collect();
        assert!(holds(&format!("dropbox:{}", hex(&Sha256::digest(&blocks))), &big));
        // An empty file has no blocks: SHA-256 of nothing (as rclone 1.74.4's `hashsum dropbox` gives).
        assert!(holds("dropbox:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", b""));
        assert!(holds(&format!("sha256:{}", hex(&Sha256::digest(&small))), &small));
        assert!(!holds(&format!("sha256:{}", hex(&Sha256::digest(b"other"))), &small));
        assert!(!holds(&format!("dropbox:{},sha256:{}", hex(&Sha256::digest(Sha256::digest(&small))), hex(&Sha256::digest(b"other"))), &small), "every hash must hold");
        assert!(!holds("md5:whatever", &small), "a kind this computer does not check never holds");
    }

    /// This machine's own roster as the cloud has it: an older copy put back at the same size is
    /// another hash, so it is downloaded and written again at the next sync; a copy listed but not
    /// downloaded (a download that broke off) is neither gone nor changed.
    #[test]
    fn this_machines_roster_put_back_is_caught_by_its_hash_and_one_unread_is_not_rewritten() {
        use crate::roster::Syncer;
        let store = Rclone::with("cloud", dropbox());
        let passphrase = crate::backup::Passphrase("synthetic roster passphrase".into());
        let t0: DateTime<Utc> = "2026-11-22T12:00:00Z".parse().unwrap();
        let at = |m: i64| t0 + chrono::Duration::minutes(m);
        let own_path = format!("cloud:SlopBar/roster/{ME}.roster");
        let mut me = Syncer::default();
        let sync = |me: &mut Syncer, percent: f64, m: i64, force: bool| {
            let from = store.runner.count();
            me.sync(&store, &passphrase, (ME, "DESK-1"), &[read_at("synthetic-a:org", percent, at(m))], at(m), force, false);
            (store.runner.ran(from, "cat"), store.runner.ran(from, "rcat"))
        };
        sync(&mut me, 10.0, 0, false);
        let old = store.runner.files.lock().unwrap()[&own_path].clone();
        assert_eq!(sync(&mut me, 60.0, 15, false), (1, 1), "a changed reading is written, and its file read back once (new, at the slow pace)");
        assert_eq!(sync(&mut me, 60.0, 25, false), (0, 0), "known: not downloaded");
        // The older copy put back, at the same size and time: another hash, downloaded, written again.
        let time = store.runner.times.lock().unwrap()[&own_path];
        assert_eq!(old.len(), store.runner.files.lock().unwrap()[&own_path].len());
        store.runner.files.lock().unwrap().insert(own_path.clone(), old.clone());
        store.runner.times.lock().unwrap().insert(own_path.clone(), time);
        assert_eq!(sync(&mut me, 60.0, 35, false), (1, 0), "found at the read");
        assert_eq!(sync(&mut me, 60.0, 40, false), (0, 1), "written again at the next sync");
        assert_eq!(me.seen.own_rewritten, 1);
        // Put back again at once: found without a download (its listed hash is that of the copy kept
        // when it was found the first time), and written over once 15 minutes have passed since the
        // last time. A read meanwhile whose download breaks off (another copy, listed so not gone,
        // unread so not changed) leaves it due.
        let watch = |me: &mut Syncer, m: i64| {
            let from = store.runner.count();
            me.sync(&store, &passphrase, (ME, "DESK-1"), &[read_at("synthetic-a:org", 60.0, at(m))], at(m), false, true);
            (store.runner.ran(from, "cat"), store.runner.ran(from, "rcat"))
        };
        store.runner.files.lock().unwrap().insert(own_path.clone(), old.clone());
        assert_eq!(watch(&mut me, 45), (0, 0), "found, from the copy kept");
        let key = crate::backup::SealKey::new(&passphrase).unwrap();
        let theirs = crate::roster::seal(&crate::roster::own(ME, "DESK-1-COPY", &[account("synthetic-a:org")], at(46)), &key).unwrap();
        store.runner.files.lock().unwrap().insert(own_path.clone(), theirs);
        store.runner.unreadable.lock().unwrap().push(own_path.clone());
        assert_eq!(watch(&mut me, 50), (1, 0), "unread: still due, and not yet");
        assert_eq!(watch(&mut me, 55), (1, 1), "written over 15 minutes after the last time");
        assert_eq!(me.seen.own_rewritten, 2);
        // Its download keeps breaking off: nothing more is written, read after read, though the next
        // wait (an hour) passes; a forced read writes, as every forced sync does, and downloads all.
        for m in [60, 70, 80] {
            assert_eq!(watch(&mut me, m), (1, 0), "{m}");
        }
        assert_eq!(sync(&mut me, 60.0, 81, true), (1, 1));
        for m in [91, 101, 111, 121] {
            assert_eq!(sync(&mut me, 60.0, m, false), (1, 0), "{m}");
        }
        assert_eq!(me.seen.own_rewritten, 2);
    }

    /// rclone processes one machine's roster syncs start in an hour, by command, beside two other
    /// machines, with a refresh every 5 minutes. `every_file` is the read before this change: every
    /// file downloaded at every read.
    fn an_hour(every_file: bool, hour: Hour) -> [usize; 3] {
        use crate::roster::{RosterStore, Syncer};
        /// The store as reads were before: no kept file stands in for a download.
        struct EveryFile<'a>(&'a Rclone<Fake>);
        impl RosterStore for EveryFile<'_> {
            fn listing(&self, _: crate::rclone::Known) -> std::io::Result<crate::rclone::Listed> { self.0.listing(&|_, _, _| None) }
            fn put(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> { RosterStore::put(self.0, name, bytes) }
            fn remove(&self, name: &str) -> std::io::Result<()> { RosterStore::remove(self.0, name) }
        }
        let store = Rclone::with("cloud", dropbox());
        let passphrase = crate::backup::Passphrase("synthetic roster passphrase".into());
        let key = crate::backup::SealKey::new(&passphrase).unwrap();
        let t0: DateTime<Utc> = "2026-11-22T12:00:00Z".parse().unwrap();
        let at = |s: i64| t0 + chrono::Duration::seconds(s);
        peer_writes(&store, &key, IDLE, "IDLE-PC", 20.0, at(-3600), None);
        peer_writes(&store, &key, BUSY, "BUSY-PC", 1.0, at(-3600), None);
        let mut me = Syncer::default();
        // Measured an hour after the app started, so its first read and write are behind it.
        me.sync(&store, &passphrase, (ME, "DESK-1"), &[read_at("synthetic-a:org", 1.0, at(-3600))], at(-3600), false, false);
        let run = |s: i64| Some(crate::roster::Run { revision: s.max(1) as u64, at: at(s), total: 9, done: 0, emails: 3, email: 1,
            finished: false, still_waiting: false, monitoring: true, everywhere: false, error: None, steps: vec![], started_at: None, open: None });
        let mut ran = [0; 3];
        // Every minute: a refresh every 5, and with Settings open on Accounts, its poll every minute.
        for s in (-3540..=3600i64).step_by(60) {
            let m = s / 60;
            match hour {
                // The busy machine writes a changed reading every 15 minutes (its gap), at 10, 25, 40…
                Hour::Busy if m.rem_euclid(15) == 10 => peer_writes(&store, &key, BUSY, "BUSY-PC", (m + 61) as f64, at(s), None),
                // A run there: a step or its heartbeat every 3 minutes.
                Hour::Running if m.rem_euclid(3) == 0 => peer_writes(&store, &key, BUSY, "BUSY-PC", 1.0, at(s), run(s)),
                _ => {}
            }
            let percent = if hour == Hour::Busy { (m + 61) as f64 } else { 1.0 };
            let accounts = [read_at("synthetic-a:org", percent, at(s))];
            let polls: &[bool] = match (hour, m.rem_euclid(5) == 0) {
                (Hour::Watching, true) => &[true, false],
                (Hour::Watching, false) => &[true],
                (_, true) => &[false],
                (_, false) => &[],
            };
            for &soon in polls {
                let from = store.runner.count();
                if every_file {
                    me.sync(&EveryFile(&store), &passphrase, (ME, "DESK-1"), &accounts, at(s), false, soon);
                } else {
                    me.sync(&store, &passphrase, (ME, "DESK-1"), &accounts, at(s), false, soon);
                }
                if s > 0 {
                    for (i, command) in ["lsjson", "cat", "rcat"].iter().enumerate() {
                        ran[i] += store.runner.ran(from, command);
                    }
                }
            }
        }
        ran
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Hour {
        /// Nothing changes anywhere.
        Quiet,
        /// This machine's reading and one other machine's change all along.
        Busy,
        /// Quiet, with Settings open on Accounts (it polls every minute, watching).
        Watching,
        /// Another machine runs "Sign in all accounts" all hour; Settings closed.
        Running,
    }

    /// The saving, measured: rclone processes an hour of roster syncs starts, before (every file
    /// downloaded at every read) and after (only files whose content changed). The listings and
    /// uploads are the same; the downloads go from one per file per read to one per changed file.
    /// `listremotes` (`kind_of`, kept `KIND_KEPT`) is not counted here.
    #[test]
    fn an_hour_of_roster_reads_over_rclone_starts_fewer_processes() {
        let mut said = Vec::new();
        for (hour, reads, after_cats, uploads) in [(Hour::Quiet, 6, 0, 0), (Hour::Busy, 6, 8, 4), (Hour::Watching, 60, 0, 0), (Hour::Running, 12, 12, 0)] {
            let (before, after) = (an_hour(true, hour), an_hour(false, hour));
            said.push(format!("{hour:?} {before:?} -> {after:?}"));
            assert_eq!((before[0], before[2]), (after[0], after[2]), "{hour:?}: the same listings and uploads");
            assert_eq!(before[0], reads, "{hour:?}: listings");
            assert_eq!(before[1], 3 * reads, "{hour:?}: before, every file at every read");
            assert_eq!(after[1], after_cats, "{hour:?}: after, the changed files only");
            assert_eq!(after[2], uploads, "{hour:?}: uploads");
        }
        eprintln!("roster rclone processes an hour [lsjson, cat, rcat], before -> after: {}", said.join("; "));
    }

    /// `machine`'s presence record as it writes it at `at`, with one account in use.
    fn record(id: &str, machine: &str, at: DateTime<Utc>) -> Presence {
        own(id, machine, 300, &[account("synthetic-a:org")], at)
    }

    /// The peers a read gives, as accounts would show them.
    fn shown(read: &[(Presence, Option<DateTime<Utc>>)], now: DateTime<Utc>) -> Vec<(String, DateTime<Utc>, bool, Vec<crate::presence::Seat>)> {
        crate::presence::peers(read.iter().cloned(), ME, now).into_iter().map(|p| (p.machine, p.seen_at, p.live, p.seats)).collect()
    }

    /// A read's records as JSON, with their times, but for this machine's own: what `others` must give
    /// where downloading every record (`list`) gives the same.
    fn raw(read: &[(Presence, Option<DateTime<Utc>>)]) -> Vec<(String, Option<DateTime<Utc>>)> {
        read.iter().filter(|(p, _)| p.machine_id != ME).map(|(p, at)| (serde_json::to_string(p).unwrap(), *at)).collect()
    }

    /// A refresh's read never downloads this machine's own record (nothing reads it), and on Dropbox
    /// and Google Drive downloads a record again only when the cloud lists it with another content
    /// hash than the copy the last read kept: a machine that went to sleep is downloaded once. What
    /// the read gives, records and times, is what downloading every record gives.
    #[test]
    fn presence_reads_skip_this_machines_record_and_records_listed_unchanged() {
        presence_reads_on(dropbox());
        presence_reads_on(Fake { kind: Some("drive".into()), ..Fake::default() });
    }

    fn presence_reads_on(fake: Fake) {
        let kind = fake.kind.clone();
        let store = Rclone::with("cloud", fake);
        let kept = Kept::default();
        let t0: DateTime<Utc> = "2026-11-23T12:00:00Z".parse().unwrap();
        let at = |m: i64| t0 + chrono::Duration::minutes(m);
        let path = |id: &str| format!("cloud:SlopBar/presence/{id}.json");
        store.write(&record(IDLE, "IDLE-PC", at(-24 * 60))).unwrap();
        // This machine writes its record, then reads; what it downloads, and what it gives.
        let refresh = |m: i64| {
            store.write(&record(ME, "DESK-1", at(m))).unwrap();
            let (from, batches) = (store.runner.count(), store.runner.batched.lock().unwrap().len());
            let read = store.others(ME, &kept).unwrap();
            let calls = store.runner.calls.lock().unwrap()[from..].to_vec();
            assert!(calls.iter().filter(|c| c.starts_with("lsjson ")).all(|c| c.contains(" --hash ")), "{kind:?}: {calls:?}");
            assert!(!calls.iter().any(|c| c.contains(ME)), "this machine's own record is never downloaded: {calls:?}");
            let batched: Vec<String> = store.runner.batched.lock().unwrap()[batches..].iter().flatten().cloned().collect();
            assert!(!batched.iter().any(|name| name.contains(ME)), "{batched:?}");
            let every = store.list().unwrap();
            assert_eq!(raw(&read), raw(&every), "{kind:?}: the records and times downloading every record gives");
            assert_eq!(shown(&read, at(m)), shown(&every, at(m)), "{kind:?}: so the same peers");
            // The records downloaded, one at a time or together.
            let mut cats: Vec<String> = calls.iter().filter_map(|c| c.strip_prefix("cat ")).filter(|c| !c.starts_with("--files-from-raw ")).map(String::from)
                .chain(batched.iter().map(|name| format!("cloud:SlopBar/presence/{name}")))
                .collect();
            cats.sort();
            (read, cats)
        };
        store.write(&record(BUSY, "BUSY-PC", at(0))).unwrap();
        let (read, cats) = refresh(0);
        assert_eq!(cats, [path(BUSY), path(IDLE)], "the first read downloads the other machines' records");
        assert_eq!(read.iter().map(|(p, _)| p.machine.as_str()).collect::<Vec<_>>(), ["BUSY-PC", "IDLE-PC"]);
        store.write(&record(BUSY, "BUSY-PC", at(5))).unwrap();
        assert_eq!(refresh(5).1, [path(BUSY)], "only the record written since");
        let (read, cats) = refresh(10);
        assert!(cats.is_empty(), "nothing written since: the listing alone");
        assert_eq!(read.len(), 2, "the kept records stand in for the downloads");
        // A rewrite in the same second at the same size is still a change: its hash is another.
        let idle = path(IDLE);
        let (second, size) = (store.runner.times.lock().unwrap()[&idle], store.runner.files.lock().unwrap()[&idle].len());
        store.runner.clock.store(second - 1, std::sync::atomic::Ordering::SeqCst);
        store.write(&record(IDLE, "IDLE-PD", at(-24 * 60))).unwrap();
        assert_eq!((store.runner.times.lock().unwrap()[&idle], store.runner.files.lock().unwrap()[&idle].len()), (second, size));
        let (read, cats) = refresh(15);
        assert_eq!((cats, read.iter().any(|(p, _)| p.machine == "IDLE-PD")), (vec![idle.clone()], true));
        assert!(refresh(20).1.is_empty());
        // Its time moving while its bytes stay (the same bytes uploaded again): the listing's new time,
        // with no download.
        let moved = store.runner.times.lock().unwrap()[&idle] + 3600;
        store.runner.times.lock().unwrap().insert(idle.clone(), moved);
        let (read, cats) = refresh(22);
        let base: DateTime<Utc> = "2026-10-21T07:00:00Z".parse().unwrap();
        assert_eq!((cats, read.iter().find(|(p, _)| p.machine_id == IDLE).unwrap().1), (vec![], Some(base + chrono::Duration::seconds(moved))));
        // A record listed with no hash is downloaded at every read, and once more when it is listed
        // with one again (nothing was kept of it without one).
        store.runner.hashless.lock().unwrap().push(idle.clone());
        assert_eq!((refresh(25).1, refresh(30).1), (vec![idle.clone()], vec![idle.clone()]));
        store.runner.hashless.lock().unwrap().clear();
        assert_eq!((refresh(35).1, refresh(40).1), (vec![idle.clone()], vec![]));
        // A record whose bytes do not have the hash it was listed with (it changed between the listing
        // and the download) is kept under none: downloaded again at the next read.
        store.runner.stale.lock().unwrap().push(idle.clone());
        assert_eq!((refresh(45).1, refresh(50).1), (vec![idle.clone()], vec![idle.clone()]));
        store.runner.stale.lock().unwrap().clear();
        assert_eq!((refresh(55).1, refresh(60).1), (vec![idle.clone()], vec![]));
        // A download that breaks off leaves the record out of that read, so `Recall` keeps the machine
        // while it is live, as for any record a read misses; the next read downloads it.
        let busy = path(BUSY);
        let mut recall = crate::presence::Recall::default();
        let place = crate::sharing::Place { remote: Some("cloud".into()), ..Default::default() };
        store.write(&record(BUSY, "BUSY-PC", at(60))).unwrap();
        let (read, cats) = refresh(61);
        assert_eq!(cats, [busy.clone()]);
        recall.fill(&place, Some(read.as_slice()), ME, at(61));
        store.write(&record(BUSY, "BUSY-PC", at(64))).unwrap();
        store.runner.unreadable.lock().unwrap().push(busy.clone());
        let (read, cats) = refresh(65);
        assert_eq!((cats, read.iter().any(|(p, _)| p.machine == "BUSY-PC")), (vec![busy.clone()], false));
        let filled = recall.fill(&place, Some(read.as_slice()), ME, at(65));
        assert!(shown(&filled, at(65)).iter().any(|(machine, _, live, _)| machine == "BUSY-PC" && *live), "remembered while live");
        store.runner.unreadable.lock().unwrap().clear();
        assert_eq!(refresh(70).1, [busy.clone()]);
        assert!(refresh(75).1.is_empty());
        // A listing that fails reads nothing and forgets nothing: the read after it still skips.
        let failing = Rclone::with("cloud", Fake { fail: Some(1), kind: kind.clone(), ..Fake::default() });
        assert!(failing.others(ME, &kept).is_err());
        assert!(refresh(80).1.is_empty());
        // Records only: a file there that is not one is downloaded at every read, as it always was.
        store.runner.files.lock().unwrap().insert(path("junk"), b"not a record".to_vec());
        assert_eq!((refresh(85).1, refresh(90).1), (vec![path("junk")], vec![path("junk")]));
    }

    /// The file under this machine's name is not read, whatever it holds: only this machine writes
    /// it, so another machine's record there (a forgery; no two SlopBar ids share their hex digits
    /// and dashes) is not shown here, as it was when every record was read.
    #[test]
    fn the_file_under_this_machines_name_is_not_read_whatever_it_holds() {
        let store = Rclone::with("cloud", dropbox());
        let forged = serde_json::to_vec(&record(BUSY, "FORGED-PC", Utc::now())).unwrap();
        store.runner.files.lock().unwrap().insert(format!("cloud:SlopBar/presence/{ME}.json"), forged);
        let from = store.runner.count();
        assert!(store.others(ME, &Kept::default()).unwrap().is_empty());
        assert_eq!((store.runner.ran(from, "lsjson"), store.runner.ran(from, "cat")), (1, 0));
        assert_eq!(store.list().unwrap().len(), 1, "every record, as before");
    }

    /// Remotes whose listing carries no hash this computer counts are not asked for one, and download
    /// every record at every read, but for this machine's own.
    #[test]
    fn presence_reads_elsewhere_download_every_other_record() {
        for kind in ["sftp", "onedrive", "box"] {
            let store = Rclone::with("cloud", Fake { kind: Some(kind.into()), ..Fake::default() });
            let kept = Kept::default();
            let now = Utc::now();
            for (id, machine) in [(ME, "DESK-1"), (BUSY, "BUSY-PC"), (IDLE, "IDLE-PC")] {
                store.write(&record(id, machine, now)).unwrap();
            }
            for _ in 0..2 {
                let from = store.runner.count();
                assert_eq!(store.others(ME, &kept).unwrap().len(), 2, "{kind}");
                let calls = store.runner.calls.lock().unwrap()[from..].to_vec();
                assert!(calls.iter().filter(|c| c.starts_with("lsjson ")).all(|c| !c.contains("--hash")), "{kind}: {calls:?}");
                assert_eq!(calls.iter().filter(|c| c.starts_with("cat ")).count(), 2, "{kind}: {calls:?}");
                assert!(!calls.iter().any(|c| c.contains(ME) || c.contains("--files-from-raw")), "one at a time, without a hash to tell them by: {kind}: {calls:?}");
            }
        }
    }

    /// Records due together on Dropbox or Google Drive come in one rclone process (one per 16), each
    /// taken from the output only by its listed size and hash; whatever that call could not give is
    /// downloaded alone when the call ended well, and left to `Recall` when it did not.
    #[test]
    fn records_due_together_are_downloaded_in_one_process() {
        let id = |n: usize| format!("{n:08x}-0000-4000-8000-000000000000");
        let path = |n: usize| format!("cloud:SlopBar/presence/{}.json", id(n));
        let now = Utc::now();
        let write = |store: &Rclone<Fake>, n: usize, at: DateTime<Utc>| store.write(&record(&id(n), &format!("PC-{n}"), at)).unwrap();
        // What a read gives and costs: (records, `cat` processes, batched calls' names). What it gives
        // is what downloading every record gives, but where a batched call timed out.
        let read = |store: &Rclone<Fake>, kept: &Kept| {
            store.write(&record(ME, "DESK-1", now)).unwrap();
            let (from, batches) = (store.runner.count(), store.runner.batched.lock().unwrap().len());
            let read = store.others(ME, kept).unwrap();
            let (cats, batched) = (store.runner.ran(from, "cat"), store.runner.batched.lock().unwrap()[batches..].to_vec());
            if !store.runner.hang.load(std::sync::atomic::Ordering::SeqCst) {
                assert_eq!(raw(&read), raw(&store.list().unwrap()), "the records and times downloading every record gives");
            }
            (read, cats, batched)
        };
        let store = Rclone::with("cloud", dropbox());
        let kept = Kept::default();
        for n in 0..20 {
            write(&store, n, now);
        }
        // Twenty due: one call for sixteen, one for four.
        let (records, cats, batched) = read(&store, &kept);
        assert_eq!((records.len(), cats, batched.iter().map(Vec::len).collect::<Vec<_>>()), (20, 2, vec![16, 4]));
        // Nothing else beside --files-from-raw, which rclone refuses with any other filter.
        assert!(store.runner.calls.lock().unwrap().iter().filter(|c| c.contains("--files-from-raw")).all(|c| c.split(' ').count() == 6 && c.ends_with(" cloud:SlopBar/presence")));
        assert_eq!(read(&store, &kept).1, 0, "nothing written since");
        for n in [3, 7, 11] {
            write(&store, n, now + chrono::Duration::seconds(1));
        }
        let (_, cats, batched) = read(&store, &kept);
        assert_eq!((cats, batched.len()), (1, 1), "three due: one process");
        write(&store, 5, now + chrono::Duration::seconds(2));
        let (_, cats, batched) = read(&store, &kept);
        assert_eq!((cats, batched.len()), (1, 0), "one due: its own `cat`");
        // One unreadable: the call ends failing; the others are read from it, that one is not, and
        // nothing is downloaded alone.
        for n in [1, 2, 4] {
            write(&store, n, now + chrono::Duration::seconds(3));
        }
        store.runner.unreadable.lock().unwrap().push(path(2));
        let (records, cats, batched) = read(&store, &kept);
        assert_eq!((records.len(), cats, batched.len()), (19, 1, 1));
        assert!(!records.iter().any(|(p, _)| p.machine_id == id(2)));
        store.runner.unreadable.lock().unwrap().clear();
        assert_eq!(read(&store, &kept).1, 1, "read the next time");
        // Changed since the listing (its bytes are not those listed), or holding the separator: not
        // taken from the call, which ended well, so downloaded alone; the others are taken.
        for n in [6, 8, 9] {
            write(&store, n, now + chrono::Duration::seconds(4));
        }
        store.runner.stale.lock().unwrap().push(path(6));
        store.runner.guessed.lock().unwrap().push(path(8));
        let (records, cats, batched) = read(&store, &kept);
        assert_eq!((records.len(), cats, batched.len()), (20, 3, 1));
        store.runner.stale.lock().unwrap().clear();
        store.runner.guessed.lock().unwrap().clear();
        let (_, cats, _) = read(&store, &kept);
        assert_eq!(cats, 1, "the one whose bytes were not those listed was not kept: downloaded again (alone)");
        assert_eq!(read(&store, &kept).1, 0, "the one that held the separator was kept");
        // A call that times out reads none of them, and none is downloaded alone; the next read does.
        for n in [10, 12] {
            write(&store, n, now + chrono::Duration::seconds(5));
        }
        store.runner.hang.store(true, std::sync::atomic::Ordering::SeqCst);
        let (records, cats, _) = read(&store, &kept);
        assert_eq!((records.len(), cats), (18, 1));
        store.runner.hang.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(read(&store, &kept).1, 1);
        // Names SlopBar does not write go one at a time.
        store.runner.files.lock().unwrap().insert("cloud:SlopBar/presence/Other Name.json".into(), serde_json::to_vec(&record(BUSY, "ODD", now)).unwrap());
        store.runner.files.lock().unwrap().insert("cloud:SlopBar/presence/odd-name.json".into(), serde_json::to_vec(&record(IDLE, "ODDER", now)).unwrap());
        let (records, cats, batched) = read(&store, &kept);
        assert_eq!((records.len(), cats, batched.len()), (22, 2, 0));

        // A call that fails having printed nothing (an rclone before 1.63 refuses `--separator` with
        // 1, as a newer one would with 2): one at a time at once; since that works, it was refused,
        // and after two such reads in a row none is made.
        for code in [1, 2] {
            let refusing = Rclone::with("cloud", Fake { refuse: Some(code), ..dropbox() });
            let kept = Kept::default();
            for second in 0..3 {
                for n in 0..3 {
                    write(&refusing, n, now + chrono::Duration::seconds(second));
                }
                let (records, cats, _) = read(&refusing, &kept);
                let expected = if second < 2 { 1 + 3 } else { 3 };
                assert_eq!((records.len(), cats), (3, expected), "{code}, read {second}");
            }
        }
        // A call that ended well having printed nothing (its files left out of it, though each can be
        // read alone) refused nothing: still batched after two such reads.
        let skipping = Rclone::with("cloud", dropbox());
        let kept = Kept::default();
        for second in 0..3 {
            for n in 0..3 {
                write(&skipping, n, now + chrono::Duration::seconds(second));
                skipping.runner.batch_skips.lock().unwrap().push(path(n));
            }
            let (records, cats, batched) = read(&skipping, &kept);
            assert_eq!((records.len(), cats, batched.len()), (3, 1 + 3, 1), "read {second}");
        }
        // Twenty due and refused: the first call only, then one at a time.
        let refusing = Rclone::with("cloud", Fake { refuse: Some(1), ..dropbox() });
        let kept = Kept::default();
        for n in 0..20 {
            write(&refusing, n, now);
        }
        let (records, cats, _) = read(&refusing, &kept);
        assert_eq!((records.len(), cats, refusing.runner.batch_calls.load(std::sync::atomic::Ordering::SeqCst)), (20, 1 + 20, 1));
        // A read in which one call gave and the next refused is no refusal: still batched after.
        let mixed = Rclone::with("cloud", Fake { refuse: Some(1), refuse_from: 1, ..dropbox() });
        let kept = Kept::default();
        for n in 0..20 {
            write(&mixed, n, now);
        }
        let (records, cats, _) = read(&mixed, &kept);
        assert_eq!((records.len(), cats), (20, 2 + 4), "sixteen given, four refused and read alone");
        assert_eq!(*mixed.runner.batching.lock().unwrap(), Batching::default());
        // Where one at a time fails too (the network), the call is not given up on.
        let store = Rclone::with("cloud", dropbox());
        let kept = Kept::default();
        for n in 0..3 {
            write(&store, n, now);
            store.runner.unreadable.lock().unwrap().push(path(n));
        }
        for _ in 0..REFUSALS {
            let (records, cats, batched) = read(&store, &kept);
            assert_eq!((records.len(), cats, batched.len()), (0, 1 + 3, 1), "printed nothing, and none read alone either");
        }
        store.runner.unreadable.lock().unwrap().clear();
        let (records, cats, batched) = read(&store, &kept);
        assert_eq!((records.len(), cats, batched.len()), (3, 1, 1), "batched again");
        assert_eq!(*store.runner.batching.lock().unwrap(), Batching::default());

        // A call that runs out of time: its records are not read that time, and the next read asks
        // for half as many at once, so a slow link still reads them.
        let store = Rclone::with("cloud", dropbox());
        let kept = Kept::default();
        for n in 0..20 {
            write(&store, n, now);
        }
        store.runner.hang.store(true, std::sync::atomic::Ordering::SeqCst);
        let (records, cats, _) = read(&store, &kept);
        assert_eq!((records.len(), cats), (0, 2));
        store.runner.hang.store(false, std::sync::atomic::Ordering::SeqCst);
        let (records, _, batched) = read(&store, &kept);
        assert_eq!((records.len(), batched.iter().map(Vec::len).collect::<Vec<_>>()), (20, vec![8, 8, 4]));
    }

    /// The pieces of a batched call go by size and hash alone: a record gone since the listing is
    /// downloaded alone (and found gone), an empty file is an empty piece, two names with the same
    /// bytes both get them, and a name in capitals is one SlopBar may write too.
    #[test]
    fn batched_pieces_go_by_size_and_hash_alone() {
        let store = Rclone::with("cloud", dropbox());
        let kept = Kept::default();
        let now = Utc::now();
        let id = |n: usize| format!("{n:08x}-0000-4000-8000-000000000000");
        let path = |name: &str| format!("cloud:SlopBar/presence/{name}");
        for n in 0..3 {
            store.write(&record(&id(n), &format!("PC-{n}"), now)).unwrap();
        }
        let copy = store.runner.files.lock().unwrap()[&path(&format!("{}.json", id(1)))].clone();
        let upper = "0000000A-0000-4000-8000-00000000000B.json";
        for (name, bytes) in [("0000000f-0000-4000-8000-000000000000.json", vec![]), ("00000010-0000-4000-8000-000000000000.json", copy.clone()), (upper, serde_json::to_vec(&record(IDLE, "CAPITALS", now)).unwrap())] {
            store.runner.files.lock().unwrap().insert(path(name), bytes);
        }
        store.runner.gone.lock().unwrap().push(path(&format!("{}.json", id(2))));
        let from = store.runner.count();
        let read = store.others(ME, &kept).unwrap();
        let alone: Vec<String> = store.runner.calls.lock().unwrap()[from..].iter().filter_map(|c| c.strip_prefix("cat ")).filter(|c| !c.starts_with("--files-from-raw")).map(String::from).collect();
        assert_eq!(alone, [path(&format!("{}.json", id(2)))], "only the one gone since the listing, downloaded alone and found gone");
        assert_eq!(store.runner.batched.lock().unwrap().last().unwrap().len(), 6, "the empty file, the copy and the name in capitals went with the rest");
        let machines: Vec<&str> = read.iter().map(|(p, _)| p.machine.as_str()).collect();
        assert_eq!(machines, ["PC-0", "PC-1", "CAPITALS", "PC-1"], "in the listing's order; the empty file is no record; the copy is read with its bytes");
        assert_eq!(raw(&read), raw(&store.list().unwrap()));
    }

    /// What a read's batched calls did sets how many the next ask for: halved after one ran out of
    /// time, none together after two reads in a row that only refused, and a read that gave clears
    /// the refusals. Each rclone (by its path) keeps its own, while SlopBar runs.
    #[test]
    fn batching_follows_how_the_calls_went() {
        let t0 = Instant::now();
        let minutes = |m: u64| t0 + Duration::from_secs(60 * m);
        let mut b = Batching::default();
        let (gave, refused, timed_out) = (Batched { gave: true, ..Default::default() }, Batched { refused: true, ..Default::default() }, Batched { timed_out: true, ..Default::default() });
        b.after(timed_out, t0);
        assert_eq!(b.at(t0).most, 8);
        b.after(refused, minutes(5));
        b.after(gave, minutes(10));
        b.after(refused, minutes(15));
        assert_eq!(b.at(minutes(15)).most, 8, "a read that gave between two refusals");
        b.after(refused, minutes(20));
        assert_eq!(b.at(minutes(20)).most, 1, "two refusals in a row");
        // An hour after it last went down it starts over: asked in full again.
        assert_eq!((b.at(minutes(79)).most, b.at(minutes(80))), (1, Batching::default()));
        let mut b = Batching::default();
        for m in 0..6 {
            b.after(timed_out, minutes(m));
        }
        assert_eq!(b.at(minutes(6)).most, 1, "halved down to one at a time");
        b.after(Batched { gave: true, refused: true, timed_out: false }, minutes(7));
        assert_eq!(b.refusals, 0);
        // Refusals far apart do not add up.
        let mut b = Batching::default();
        b.after(refused, t0);
        b.after(refused, minutes(61));
        assert_eq!((b.at(minutes(61)).most, b.refusals), (BATCH_MOST, 1));
        // Kept by the rclone's path, for every Cli of it.
        let dir = crate::test_support::TempDir::new();
        let (one, other) = (dir.0.join("rclone-batching-one.exe"), dir.0.join("rclone-batching-other.exe"));
        for _ in 0..REFUSALS {
            Cli::local(one.clone()).batched(refused);
        }
        assert_eq!((Cli::local(one).batch_most(), Cli::local(other).batch_most()), (1, BATCH_MOST));
    }

    #[test]
    fn output_is_cut_at_each_separator() {
        assert_eq!(split(b"a--b----c--", b"--"), [b"a".to_vec(), b"b".to_vec(), vec![], b"c".to_vec(), vec![]]);
        assert_eq!(split(b"", b"--"), [Vec::<u8>::new()]);
        assert_eq!(split(b"cut off", b"--"), [b"cut off".to_vec()]);
        for (name, shaped) in [("0f3a-b9.json", true), ("0F3A.json", true), ("a b.json", false), (".json", false), ("abc.roster", false), ("ab\nc.json", false), ("abg.json", false), ("ab/c.json", false), ("..json", false)] {
            assert_eq!(record_name_shaped(name, "json"), shaped, "{name:?}");
        }
    }

    /// How a presence read downloads what is due (`presence_hour`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Downloads {
        /// Every record, at every read: before these changes (`list`).
        EveryRecord,
        /// The records due, one `cat` each: on an rclone that refuses batched calls.
        OneAtATime,
        /// The records due, together.
        Together,
    }

    /// rclone processes an hour of one machine's presence exchanges starts, refreshing every 5
    /// minutes beside `live` machines that write at each of theirs and `asleep` ones that stopped:
    /// [lsjson, cat, rcat].
    fn presence_hour(downloads: Downloads, live: usize, asleep: usize) -> [usize; 3] {
        let store = Rclone::with("cloud", Fake { refuse: (downloads == Downloads::OneAtATime).then_some(2), ..dropbox() });
        let every_file = downloads == Downloads::EveryRecord;
        let kept = Kept::default();
        let t0: DateTime<Utc> = "2026-11-23T12:00:00Z".parse().unwrap();
        let id = |n: usize| format!("{n:08x}-0000-4000-8000-000000000000");
        for n in 0..asleep {
            store.write(&record(&id(100 + n), &format!("ASLEEP-{n}"), t0 - chrono::Duration::days(2))).unwrap();
        }
        let mut ran = [0; 3];
        // An hour since SlopBar started, then the hour measured.
        for m in (-60..60i64).step_by(5) {
            let now = t0 + chrono::Duration::minutes(m);
            for n in 0..live {
                store.write(&record(&id(n), &format!("LIVE-{n}"), now - chrono::Duration::seconds(40))).unwrap();
            }
            let from = store.runner.count();
            store.write(&record(ME, "DESK-1", now)).unwrap();
            let read = if every_file { store.list() } else { store.others(ME, &kept) };
            assert_eq!(read.unwrap().len(), live + asleep + every_file as usize);
            if m >= 0 {
                for (i, command) in ["lsjson", "cat", "rcat"].iter().enumerate() {
                    ran[i] += store.runner.ran(from, command);
                }
            }
        }
        ran
    }

    /// The saving, modelled: a refresh's downloads go from every record (this machine's own
    /// included) to the records written since the last read, then to one process for those. Live
    /// machines rewrite theirs at every refresh, so they are due at every read; asleep and retired
    /// machines' records, and this machine's own, are not. The listings and uploads are the same.
    #[test]
    fn an_hour_of_presence_exchanges_over_rclone_starts_fewer_processes() {
        let mut said = Vec::new();
        // ULTRA-MAGNUS's fleet on 2026-11-23 (five others live, two asleep), then one beside two
        // live machines only, one beside two asleep ones only, and one beside a single live one.
        for (live, asleep) in [(5, 2), (2, 0), (0, 2), (1, 0)] {
            let [before, one, together] = [Downloads::EveryRecord, Downloads::OneAtATime, Downloads::Together].map(|d| presence_hour(d, live, asleep));
            said.push(format!("{live} live, {asleep} asleep: {before:?} -> {one:?} -> {together:?}"));
            assert_eq!(before, [12, 12 * (1 + live + asleep), 12], "{live} live, {asleep} asleep: before, every record at every read");
            assert_eq!(one, [12, 12 * live, 12], "{live} live, {asleep} asleep: the records written since, one at a time");
            assert_eq!(together, [12, 12 * live.min(1), 12], "{live} live, {asleep} asleep: the records written since, together");
        }
        eprintln!("presence rclone processes an hour [lsjson, cat, rcat], before -> one at a time -> together: {}", said.join("; "));
    }

    /// A remote's kind (one `listremotes` each time it is looked up) is kept half an hour, so a sync
    /// every minute with Settings open does not look it up every time; a command that fails forgets
    /// it, so a remote pointed at another cloud is looked up again at once.
    #[test]
    fn a_remotes_kind_is_kept_half_an_hour_and_forgotten_when_a_command_fails() {
        // A name of this test's own: the kinds are kept for the whole process.
        let remote = "kept-kind-test";
        let t0 = Instant::now();
        let looked = std::cell::Cell::new(0);
        let look = |kind: &'static str| {
            let looked = &looked;
            move || {
                looked.set(looked.get() + 1);
                Some(kind.to_string())
            }
        };
        let minutes = |m: u64| t0 + Duration::from_secs(60 * m);
        assert_eq!(kept_kind(remote, t0, look("dropbox")).as_deref(), Some("dropbox"));
        assert_eq!(kept_kind(remote, minutes(29), look("drive")).as_deref(), Some("dropbox"));
        assert_eq!(looked.get(), 1, "kept: no listremotes");
        assert_eq!(kept_kind(remote, minutes(31), look("drive")).as_deref(), Some("drive"));
        // A listing on the remote that fails (here rclone is missing) forgets it; a download that
        // fails does not (it follows a listing that worked).
        let dir = crate::test_support::TempDir::new();
        let missing = dir.0.join(if cfg!(windows) { "rclone.exe" } else { "rclone" });
        let cli = Cli::routed(missing, remote, crate::test_support::router(None, &[]));
        assert!(cli.run(&["cat", "kept-kind-test:SlopBar/roster/x.roster"], None).is_err());
        assert_eq!(kept_kind(remote, minutes(31) + Duration::from_secs(30), look("box")).as_deref(), Some("drive"));
        assert!(cli.run(&["lsjson", "--files-only", "--no-mimetype", "kept-kind-test:SlopBar/roster"], None).is_err());
        assert_eq!(kept_kind(remote, minutes(32), look("onedrive")).as_deref(), Some("onedrive"));
        assert_eq!(looked.get(), 3);
    }

    #[test]
    fn failures_show_nothing_rather_than_guessing() {
        let store = Rclone::with("cloud", Fake { fail: Some(1), ..Default::default() });
        assert!(store.list().is_err());
        assert!(store.write(&own("11111111-1111-4111-8111-111111111111", "B", 300, &[], Utc::now())).is_err());
        assert!(read(&store, "me", Utc::now()).is_empty());
    }

    /// Where the network's proxy cannot be used, rclone is not started, and what Health and Settings
    /// show says so: the presence exchange's and the roster's writes and reads, each with the
    /// proxy's code. A later failure for another reason, or a success, takes the code away.
    #[test]
    fn sharing_stopped_by_the_networks_proxy_says_why() {
        use crate::proxy::Refusal;
        use crate::roster::Syncer;
        const MACHINE: &str = "11111111-1111-4111-8111-111111111111";
        // An rclone that does not exist: one started would fail, and say nothing of a proxy.
        let dir = crate::test_support::TempDir::new();
        let missing = dir.0.join("no-such-folder").join(if cfg!(windows) { "rclone.exe" } else { "rclone" });
        let passphrase = crate::backup::Passphrase("synthetic roster passphrase".into());
        let accounts = [account("synthetic-a:org")];
        for (refusal, env, code) in [
            (Some(Refusal::Script), &[][..], "proxy_script"),
            (Some(Refusal::Unsupported), &[][..], "proxy_unsupported"),
            (None, &[("HTTPS_PROXY", "socks5://socks.example.test:1080")][..], "proxy_environment"),
        ] {
            let store = Rclone::with("dropbox", Cli::routed(missing.clone(), "dropbox", crate::test_support::router(refusal, env)));
            let now = Utc::now();
            let wrote = store.write(&own(MACHINE, "DESK-1", 300, &accounts, now));
            let listed = PresenceStore::list(&store);
            let shared = crate::health::SharingResult::of(now, &wrote, &listed);
            assert_eq!((shared.wrote, shared.read, shared.write_error, shared.read_error), (false, false, Some(code), Some(code)));
            let mut sync = Syncer::default();
            sync.sync(&store, &passphrase, (MACHINE, "DESK-1"), &accounts, now, false, false);
            let seen = &sync.seen;
            assert_eq!((seen.wrote, seen.write_error, seen.read, seen.read_error), (Some(false), Some(code), Some(false), Some(code)));

            // Routes that work again: rclone starts (and, missing, fails for another reason).
            let direct = Rclone::with("dropbox", Cli::routed(missing.clone(), "dropbox", crate::test_support::router(None, &[])));
            let wrote = direct.write(&own(MACHINE, "DESK-1", 300, &accounts, now));
            assert!(wrote.is_err() && crate::proxy::refusal_code(wrote.as_ref().unwrap_err()).is_none(), "{wrote:?}");
            let shared = crate::health::SharingResult::of(now, &wrote, &PresenceStore::list(&direct));
            assert_eq!((shared.write_error, shared.read_error), (None, None));
            sync.sync(&direct, &passphrase, (MACHINE, "DESK-1"), &accounts, now, true, false);
            let seen = &sync.seen;
            assert_eq!((seen.wrote, seen.write_error, seen.read, seen.read_error), (Some(false), None, Some(false), None));
            // And a sharing location that works clears it.
            sync.sync(&Rclone::with("dropbox", Fake::default()), &passphrase, (MACHINE, "DESK-1"), &accounts, now, true, false);
            let seen = &sync.seen;
            assert_eq!((seen.wrote, seen.write_error, seen.read, seen.read_error), (Some(true), None, Some(true), None));
        }
    }

    #[test]
    fn lists_remotes_by_type_only() {
        let fake = Fake { remotes: "dropbox: dropbox\nwork drive:   drive\n-evil: s3\nodd:\n".into(), ..Default::default() };
        let found = remotes(&fake);
        assert_eq!(found, [
            Remote { name: "dropbox".into(), kind: "dropbox".into(), provider: "Dropbox".into() },
            Remote { name: "work drive".into(), kind: "drive".into(), provider: "Google Drive".into() },
            Remote { name: "odd".into(), kind: "".into(), provider: "rclone".into() },
        ]);
        assert_eq!(*fake.calls.lock().unwrap(), ["listremotes --long"], "never `config dump`");
        assert!(remotes(&Fake { fail: Some(1), ..Default::default() }).is_empty());
    }

    #[test]
    fn remote_names_cannot_become_flags() {
        for bad in ["", "-x", "--config", " lead", "a/b", "a:b", "a\nb", &"x".repeat(65)] {
            assert!(!valid_remote(bad), "{bad:?}");
        }
        for good in ["dropbox", "work drive", "my.remote+2", "a@b_c-d"] {
            assert!(valid_remote(good), "{good:?}");
        }
    }

    /// Against a real remote, touching only `SlopBar/presence`:
    /// `SLOPBAR_RCLONE_LIVE=dropbox cargo test --lib rclone_live -- --ignored`
    #[test]
    #[ignore]
    fn rclone_live() {
        let remote = std::env::var("SLOPBAR_RCLONE_LIVE").expect("SLOPBAR_RCLONE_LIVE names a remote");
        let store = Rclone::open(&remote).expect("rclone installed and a valid remote");
        let id = "00000000-0000-4000-8000-00000000c0de";
        let record = own(id, "SLOPBAR-LIVE-TEST", 300, &[account("synthetic-live:org")], Utc::now());
        store.write(&record).unwrap();
        let listed = store.list().unwrap();
        let back = listed.iter().find(|(p, _)| p.machine_id == id).expect("written record listed");
        assert_eq!(back.0.seats, record.seats);
        assert!(back.1.is_some(), "rclone reports a modification time");
        store.clear(id, "SLOPBAR-LIVE-TEST", 300).unwrap();
        let cleared = store.list().unwrap();
        assert!(cleared.iter().find(|(p, _)| p.machine_id == id).unwrap().0.seats.is_empty());
        // Leave nothing behind.
        store.run(&["deletefile", &store.path(&format!("{id}.json"))], None).unwrap();
        assert!(store.list().unwrap().iter().all(|(p, _)| p.machine_id != id));
    }
}
