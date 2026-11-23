//! Multiple accounts per provider.
//!
//! Each CLI signs in one account at a time, in its default config location. SlopBar keeps a
//! vault with a saved login ("slot") for every account it has seen there, and switches accounts
//! by writing a slot back into those default files — the official CLI keeps doing all the work,
//! nothing is proxied.
//!
//! Tokens rotate when a CLI refreshes them, so the live login is always synced back into its
//! slot before anything else is written. New accounts are added by running the CLI's own login
//! in an isolated, temporary config dir and importing the result.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::sources::{self, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }

    pub(crate) fn key(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

/// A saved login: everything a CLI needs to be signed in as one account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slot {
    pub id: String,
    pub provider: Provider,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub org: Option<String>,
    /// Claude: `{ credentials: {claudeAiOauth, organizationUuid}, oauth_account }`.
    /// Codex: `{ auth: <auth.json> }`.
    pub data: Value,
}

impl Slot {
    /// Access token and, for Codex, the ChatGPT account id used for usage requests.
    pub fn access(&self) -> Option<(String, Option<String>)> {
        match self.provider {
            Provider::Claude => {
                let oauth = &self.data["credentials"]["claudeAiOauth"];
                Some((oauth["accessToken"].as_str()?.to_string(), None))
            }
            Provider::Codex => {
                let tokens = &self.data["auth"]["tokens"];
                Some((
                    tokens["access_token"].as_str()?.to_string(),
                    tokens["account_id"].as_str().map(String::from),
                ))
            }
        }
    }

    /// Plan and billing facts the saved login carries: Claude's rate-limit tier and account record,
    /// Codex's subscription claims in its id token. Never a token, email or account id.
    pub fn billing(&self) -> crate::model::BillingHints {
        let date = |v: &Value| v.as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&chrono::Utc));
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(String::from);
        match self.provider {
            Provider::Claude => {
                let account = &self.data["oauth_account"];
                crate::model::BillingHints {
                    tier: text(&self.data["credentials"]["claudeAiOauth"]["rateLimitTier"]).or_else(|| text(&account["organizationRateLimitTier"])),
                    billing_type: text(&account["billingType"]),
                    started_at: date(&account["subscriptionCreatedAt"]),
                    ..Default::default()
                }
            }
            Provider::Codex => {
                let claims = self.data["auth"]["tokens"]["id_token"].as_str().and_then(jwt_claims).unwrap_or(Value::Null);
                let openai = &claims["https://api.openai.com/auth"];
                crate::model::BillingHints {
                    started_at: date(&openai["chatgpt_subscription_active_start"]),
                    active_until: date(&openai["chatgpt_subscription_active_until"]),
                    checked_at: date(&openai["chatgpt_subscription_last_checked"]),
                    ..Default::default()
                }
            }
        }
    }

    /// Whether the saved access token has certainly expired (Claude records its expiry).
    pub fn expired(&self) -> bool {
        match self.provider {
            Provider::Claude => self.data["credentials"]["claudeAiOauth"]["expiresAt"]
                .as_i64()
                .is_some_and(|ms| ms < chrono::Utc::now().timestamp_millis()),
            Provider::Codex => false,
        }
    }

    /// When this saved sign-in ends: the date the provider set when it was made, which renewing
    /// never moves (Claude ends every sign-in about 30 days after it is made; Claude Code saves the
    /// date as `refreshTokenExpiresAt`, in milliseconds). None when the login does not say (Codex,
    /// or a login saved by an older Claude Code), or says something no sign-in could mean: a zero,
    /// seconds instead of milliseconds, or a date years away, which would otherwise mark every
    /// account's sign-in as ended.
    pub fn ends_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let at = match self.provider {
            Provider::Claude => self.data["credentials"]["claudeAiOauth"]["refreshTokenExpiresAt"]
                .as_i64()
                .and_then(chrono::DateTime::from_timestamp_millis)?,
            Provider::Codex => return None,
        };
        let earliest = chrono::DateTime::from_timestamp(1_704_067_200, 0)?; // 2024-01-01
        (at >= earliest && at <= chrono::Utc::now() + chrono::Duration::days(400)).then_some(at)
    }
}

/// Whether an account's saved CLI sign-in, which ends at `ends` (`Slot::ends_at`), needs a new
/// one: it ended, so switching the CLI to it would stop the CLI until it signs in again, or it ends
/// within `monitoring::RENEW_DAYS`, so sign-ins ending a few days apart are made again in one
/// sitting. None while the account is in use (`active`): the CLI manages that sign-in, renews it
/// and says itself when to sign in again. SlopBar never renews a saved CLI sign-in (its refresh
/// token never goes to a token endpoint), so there is no refusal to go by, only the date.
pub fn cli_renewal(ends: Option<chrono::DateTime<chrono::Utc>>, active: bool, now: chrono::DateTime<chrono::Utc>) -> Option<crate::model::RenewalKind> {
    use crate::model::RenewalKind;
    let at = ends.filter(|_| !active)?;
    if at <= now {
        Some(RenewalKind::Ended)
    } else if at <= now + chrono::Duration::days(crate::monitoring::RENEW_DAYS) {
        Some(RenewalKind::Ending)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Reading and writing a CLI's login files

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Writes through a temp file and a rename so a CLI never reads a half-written file.
fn write_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let tmp = path.with_extension("slopbar-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&tmp, path)
}

fn stable_hash(s: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Claude account identities resolved from the profile API, keyed by access token hash.
/// `.claude.json` can't be trusted for identity: a running Claude Code rewrites it from memory
/// and may restore the previous account's details right after a switch.
static CLAUDE_IDENTITY: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);

fn claude_token(source: &Source) -> Option<String> {
    let creds = read_json(&source.claude_dir().join(".credentials.json"))?;
    creds["claudeAiOauth"]["accessToken"].as_str().map(String::from)
}

/// Resolves (once per token) who each live Claude login belongs to.
pub async fn resolve_identities(client: &reqwest::Client, sources: &[Source]) {
    let mut unknown: Vec<_> = sources.iter().filter_map(claude_token).map(|token| (stable_hash(&token), token))
        .filter(|(key, _)| !CLAUDE_IDENTITY.lock().unwrap().as_ref().is_some_and(|m| m.contains_key(key)))
        .collect();
    unknown.sort_by(|a, b| a.0.cmp(&b.0));
    unknown.dedup_by(|a, b| a.0 == b.0);
    // Read-only profile lookups: all at once, so one slow login does not hold up the others.
    let found = futures::future::join_all(unknown.into_iter().map(|(key, token)| async move {
        claude_profile(client, &token).await.map(|account| (key, account))
    })).await;
    let mut known = CLAUDE_IDENTITY.lock().unwrap();
    known.get_or_insert_with(HashMap::new).extend(found.into_iter().flatten());
}

/// The login currently signed in at `source`, if any. `known` are the vault's slots.
///
/// Claude's identity is taken, in order of trust, from: a vault slot holding this exact refresh
/// token (right after a switch), the profile API for this access token, and only then
/// `.claude.json` (which a running Claude Code may have rewritten with a previous account).
pub fn read_login(provider: Provider, source: &Source, known: &[Slot]) -> Option<Slot> {
    match provider {
        Provider::Claude => {
            let creds = read_json(&source.claude_dir().join(".credentials.json"))?;
            let oauth = creds.get("claudeAiOauth")?;
            let token = oauth.get("accessToken")?.as_str()?;
            let refresh = oauth["refreshToken"].as_str().unwrap_or_default();
            let file = read_json(&source.claude_json())
                .and_then(|j| j.get("oauthAccount").cloned())
                .filter(|a| a.get("accountUuid").is_some());

            let same_login = known.iter().find(|s| {
                s.provider == Provider::Claude
                    && !refresh.is_empty()
                    && s.data["credentials"]["claudeAiOauth"]["refreshToken"].as_str() == Some(refresh)
            });
            let resolved = CLAUDE_IDENTITY
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|m| m.get(&stable_hash(token)).cloned());
            let identity = same_login
                .map(|s| s.data["oauth_account"].clone())
                .filter(|a| !a.is_null())
                .or(resolved);
            let account = match identity {
                // Prefer the CLI's full record when it agrees with the trusted identity.
                Some(id) => Some(match &file {
                    Some(f) if f["accountUuid"] == id["accountUuid"] => f.clone(),
                    _ => id,
                }),
                None => file,
            };
            claude_slot(creds.clone(), account)
        }
        Provider::Codex => {
            let _ = known;
            let auth = read_json(&source.codex_dir().join("auth.json"))?;
            codex_slot(auth)
        }
    }
}

fn claude_slot(creds: Value, account: Option<Value>) -> Option<Slot> {
    let oauth = creds.get("claudeAiOauth")?;
    let org = creds["organizationUuid"]
        .as_str()
        .or_else(|| account.as_ref().and_then(|a| a["organizationUuid"].as_str()))
        .unwrap_or("");
    // Identity must survive token rotation: account + organization when known.
    let id = match account.as_ref().and_then(|a| a["accountUuid"].as_str()) {
        Some(uuid) => format!("{uuid}:{org}"),
        None => format!("token:{}", stable_hash(oauth["refreshToken"].as_str().unwrap_or(""))),
    };
    Some(Slot {
        id,
        provider: Provider::Claude,
        email: account.as_ref().and_then(|a| a["emailAddress"].as_str().map(String::from)),
        plan: oauth["subscriptionType"].as_str().map(String::from),
        org: account.as_ref().and_then(|a| a["organizationName"].as_str().map(String::from)),
        data: json!({
            "credentials": {
                "claudeAiOauth": oauth,
                "organizationUuid": creds.get("organizationUuid").cloned().unwrap_or(Value::Null),
            },
            "oauth_account": account.unwrap_or(Value::Null),
        }),
    })
}

fn codex_slot(auth: Value) -> Option<Slot> {
    let tokens = auth.get("tokens")?;
    tokens.get("access_token")?;
    let claims = tokens["id_token"].as_str().and_then(jwt_claims).unwrap_or(Value::Null);
    let openai = &claims["https://api.openai.com/auth"];
    let user = openai["chatgpt_user_id"]
        .as_str()
        .or(openai["user_id"].as_str())
        .or(claims["sub"].as_str())
        .unwrap_or("");
    let account = tokens["account_id"].as_str().or(openai["chatgpt_account_id"].as_str()).unwrap_or("");
    let id = if user.is_empty() && account.is_empty() {
        format!("token:{}", stable_hash(tokens["refresh_token"].as_str().unwrap_or("")))
    } else {
        format!("{user}:{account}")
    };
    Some(Slot {
        id,
        provider: Provider::Codex,
        email: claims["email"].as_str().map(String::from),
        plan: openai["chatgpt_plan_type"].as_str().map(String::from),
        org: None,
        data: json!({ "auth": auth }),
    })
}

/// Signs `source` in as `slot`, touching only account-specific fields.
fn write_login(slot: &Slot, source: &Source) -> std::io::Result<()> {
    match slot.provider {
        Provider::Claude => {
            let path = source.claude_dir().join(".credentials.json");
            let mut creds = read_json(&path).unwrap_or_else(|| json!({}));
            creds["claudeAiOauth"] = slot.data["credentials"]["claudeAiOauth"].clone();
            creds["organizationUuid"] = slot.data["credentials"]["organizationUuid"].clone();
            write_json(&path, &creds)?;
            // The rest of .claude.json is user config and history; only the account block changes.
            let config = source.claude_json();
            if let (Some(mut cfg), false) = (read_json(&config), slot.data["oauth_account"].is_null()) {
                cfg["oauthAccount"] = slot.data["oauth_account"].clone();
                write_json(&config, &cfg)?;
            }
            Ok(())
        }
        Provider::Codex => write_json(&source.codex_dir().join("auth.json"), &slot.data["auth"]),
    }
}

/// Whether the provider's CLI has been used at `source` (its config dir exists).
fn installed(provider: Provider, source: &Source) -> bool {
    match provider {
        Provider::Claude => source.claude_dir().is_dir(),
        Provider::Codex => source.codex_dir().is_dir(),
    }
}

// ---------------------------------------------------------------------------------------------
// The vault
//
// One file per slot, encrypted with Windows DPAPI for the signed-in Windows user (see
// `private_store::save_secret`): `<provider>-<id>.slot`. Releases before the encrypted vault
// wrote the same login as plaintext `<provider>-<id>.json`; `migrate_vault` encrypts those on
// startup. For one release a plaintext slot is still read (whichever file is newer wins), so a
// slot whose migration failed is never lost, only left unencrypted until the next attempt.

fn vault_dir() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("accounts"))
}

fn slot_stem(provider: Provider, id: &str) -> String {
    let safe: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    format!("{}-{safe}", provider.key())
}

fn slot_path_in(vault: &Path, provider: Provider, id: &str) -> PathBuf {
    vault.join(format!("{}.{SLOT_EXT}", slot_stem(provider, id)))
}

const SLOT_EXT: &str = "slot";
/// The plaintext slots of earlier releases, read for one release as a fallback.
const PLAIN_EXT: &str = "json";

pub fn load_slots() -> Vec<Slot> {
    vault_dir().map(|d| load_slots_in(&d)).unwrap_or_default()
}

fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_slot(path: &Path) -> Option<Slot> {
    let bytes = match path.extension().and_then(|e| e.to_str()) {
        Some(SLOT_EXT) => crate::private_store::load_secret(path).ok()?,
        Some(PLAIN_EXT) => std::fs::read(path).ok()?,
        _ => return None,
    };
    serde_json::from_slice(&bytes).ok()
}

fn load_slots_in(dir: &Path) -> Vec<Slot> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    // Per file stem, the newest readable copy: an encrypted slot, or a plaintext one written by an
    // earlier release (after a downgrade, the plaintext copy is the newer login).
    let mut newest: HashMap<String, (Option<std::time::SystemTime>, Slot)> = HashMap::new();
    for path in entries.flatten().map(|e| e.path()) {
        let (Some(stem), Some(slot)) = (path.file_stem().and_then(|s| s.to_str()).map(String::from), read_slot(&path)) else { continue };
        let at = modified(&path);
        if newest.get(&stem).map_or(true, |(t, _)| at > *t) {
            newest.insert(stem, (at, slot));
        }
    }
    let mut slots: Vec<Slot> = newest.into_values().map(|(_, s)| s).collect();
    slots.sort_by(|a, b| (a.provider.key(), &a.email, &a.id).cmp(&(b.provider.key(), &b.email, &b.id)));
    slots
}

/// Saves a login from `LoginJob::wait_isolated` in the vault, as `LoginJob::wait` does. A new
/// sign-in for an account the vault already has replaces its saved login, keeping the account
/// details only the old one carried (`keep_details`).
pub fn store_login(slot: &Slot) -> std::io::Result<()> {
    let _guard = VAULT.lock().unwrap();
    let vault = vault_dir().ok_or_else(|| std::io::Error::other("no config dir"))?;
    store_login_in(&vault, slot)
}

fn store_login_in(vault: &Path, slot: &Slot) -> std::io::Result<()> {
    let mut slot = slot.clone();
    if let Some(old) = load_slots_in(vault).into_iter().find(|s| s.provider == slot.provider && s.id == slot.id) {
        keep_details(&mut slot, &old);
    }
    save_slot_in(vault, &slot)
}

/// What `store_renewal` did with a new CLI sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renewed {
    /// It replaced the saved login.
    Saved,
    /// The account is now signed in where the CLI runs: the CLI manages that login, and the next
    /// `capture` would put the live one back over a new one. Nothing was saved.
    InUse,
    /// The saved login was removed meanwhile, and a removed account stays removed. Nothing was saved.
    Removed,
}

/// Saves a new CLI sign-in for an account the vault already has, made because its saved one ended
/// or ends soon (see `cli_renewal`), as `store_login` does; but only while the account is still
/// saved and still not signed in at any of `sources`, both checked under the vault lock, so neither
/// a switch nor a removal during the sign-in is undone. Nothing is signed out either way: an unsaved
/// sign-in simply runs out.
pub fn store_renewal(slot: &Slot, sources: &[Source]) -> std::io::Result<Renewed> {
    let _guard = VAULT.lock().unwrap();
    let vault = vault_dir().ok_or_else(|| std::io::Error::other("no config dir"))?;
    store_renewal_in(&vault, slot, sources)
}

fn store_renewal_in(vault: &Path, slot: &Slot, sources: &[Source]) -> std::io::Result<Renewed> {
    let known = load_slots_in(vault);
    if !known.iter().any(|s| s.provider == slot.provider && s.id == slot.id) {
        return Ok(Renewed::Removed);
    }
    if sources.iter().any(|source| read_login(slot.provider, source, &known).is_some_and(|live| live.id == slot.id)) {
        return Ok(Renewed::InUse);
    }
    store_login_in(vault, slot)?;
    Ok(Renewed::Saved)
}

/// A new Claude sign-in's account record comes from the profile API, which returns fewer fields
/// than Claude Code saves (billingType and subscriptionCreatedAt, which `Slot::billing` reads,
/// among them): the old login's record fills in what the new one lacks, for the same account and
/// organization only (the CLI's own record can be another organization's of the same person). The
/// new record's own fields win. Claude Code replaces the whole record once the account is in use
/// again.
fn keep_details(new: &mut Slot, old: &Slot) {
    if new.provider != Provider::Claude || new.provider != old.provider || new.id != old.id {
        return;
    }
    let same_as = |key: &str| new.data["oauth_account"][key].as_str().is_some_and(|v| Some(v) == old.data["oauth_account"][key].as_str());
    let same = same_as("accountUuid") && same_as("organizationUuid");
    if let (true, Some(fresh), Some(saved)) = (same, new.data["oauth_account"].as_object_mut(), old.data["oauth_account"].as_object()) {
        for (key, value) in saved {
            fresh.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
}

fn save_slot(slot: &Slot) -> std::io::Result<()> {
    save_slot_in(&vault_dir().ok_or_else(|| std::io::Error::other("no config dir"))?, slot)
}

fn save_slot_in(vault: &Path, slot: &Slot) -> std::io::Result<()> {
    let path = slot_path_in(vault, slot.provider, &slot.id);
    crate::private_store::save_secret(&path, &serde_json::to_vec(slot)?)?;
    // The encrypted copy is now the newest, so a leftover plaintext one is only a stale secret.
    match std::fs::remove_file(path.with_extension(PLAIN_EXT)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub fn remove_slot(provider: Provider, id: &str) -> std::io::Result<()> {
    let Some(vault) = vault_dir() else { return Ok(()) };
    let path = slot_path_in(&vault, provider, id);
    let mut removed = false;
    for path in [path.with_extension(PLAIN_EXT), path] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if removed { Ok(()) } else { Err(std::io::ErrorKind::NotFound.into()) }
}

/// What encrypting the plaintext slots of earlier releases did (see `migrate_vault`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VaultMigration {
    /// Plaintext slots now stored encrypted, their plaintext file removed.
    pub migrated: usize,
    /// Slots that could not be encrypted, or did not read back; their plaintext file was kept.
    pub failed: usize,
    /// Plaintext files that are not a readable slot; left untouched.
    pub unreadable: usize,
}

impl VaultMigration {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Encrypts every plaintext slot. Each one is written encrypted, read back and compared, and only
/// then is its plaintext file removed; on any failure the plaintext file stays (and is still read)
/// and a half-written encrypted copy is removed, so no slot is ever lost.
pub fn migrate_vault() -> VaultMigration {
    let _guard = VAULT.lock().unwrap_or_else(|e| e.into_inner());
    vault_dir().map(|d| migrate_vault_in(&d, crate::private_store::save_secret, crate::private_store::load_secret)).unwrap_or_default()
}

fn migrate_vault_in(
    vault: &Path,
    save: impl Fn(&Path, &[u8]) -> std::io::Result<()>,
    load: impl Fn(&Path) -> std::io::Result<Vec<u8>>,
) -> VaultMigration {
    let mut result = VaultMigration::default();
    let Ok(entries) = std::fs::read_dir(vault) else { return result };
    let mut plain: Vec<PathBuf> = entries.flatten().map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(PLAIN_EXT))
        .collect();
    plain.sort();
    for path in plain {
        let Some(slot) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).filter(|v| serde_json::from_value::<Slot>(v.clone()).is_ok()) else {
            result.unreadable += 1;
            continue;
        };
        let target = path.with_extension(SLOT_EXT);
        // A newer encrypted copy already exists: the plaintext one is stale, but it is only
        // removed once that copy is known to read back.
        let newer = target.exists() && modified(&target) > modified(&path)
            && load(&target).ok().and_then(|b| serde_json::from_slice::<Slot>(&b).ok()).is_some();
        let stored = newer || (|| -> std::io::Result<bool> {
            save(&target, &serde_json::to_vec(&slot)?)?;
            Ok(load(&target).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).as_ref() == Some(&slot))
        })().unwrap_or(false);
        if stored && std::fs::remove_file(&path).is_ok() {
            result.migrated += 1;
        } else {
            if !stored {
                // Whatever encrypted copy is there now is either half-written or older than the
                // plaintext one, which stays and keeps being read.
                let _ = std::fs::remove_file(&target);
            }
            result.failed += 1;
        }
    }
    result
}

/// Plaintext slot files still in the vault (a migration that failed, or an earlier release
/// running alongside).
pub fn plaintext_slots() -> usize {
    let Some(Ok(entries)) = vault_dir().map(std::fs::read_dir) else { return 0 };
    entries.flatten().filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(PLAIN_EXT)).count()
}

/// Serializes vault reads/writes against switches.
static VAULT: Mutex<()> = Mutex::new(());

/// Where each account is currently signed in: `(provider, slot id) -> source labels`.
pub type Active = HashMap<(Provider, String), Vec<String>>;

/// Saves every live login into the vault (new accounts appear here; rotated tokens are kept
/// current) and reports which account is signed in where.
pub fn capture(sources: &[Source]) -> Active {
    let _guard = VAULT.lock().unwrap();
    let known = load_slots();
    let mut active = Active::new();
    for provider in Provider::ALL {
        for source in sources {
            if let Some(slot) = read_login(provider, source, &known) {
                let _ = save_slot(&slot);
                active.entry((provider, slot.id.clone())).or_default().push(source.label.clone());
            }
        }
    }
    active
}

/// Signs every source where the provider is installed into the account `id`.
pub fn switch(sources: &[Source], provider: Provider, id: &str) -> Result<(), String> {
    // Sync live logins first: the account being left may have refreshed its tokens.
    capture(sources);
    // Where no source has the CLI's folder: a CLI installed on Windows but not run yet gets one,
    // signed in to this account (never for the Claude desktop app's own copy: the app makes its
    // folder). Looked for before the vault is locked: it can wait on an offline network folder.
    let unused_cli = sources.iter().all(|s| !installed(provider, s))
        && crate::cli::find(provider).is_some_and(|f| f.kind != crate::cli::Kind::DesktopApp);
    let _guard = VAULT.lock().unwrap();
    let mut target = load_slots()
        .into_iter()
        .find(|s| s.provider == provider && s.id == id)
        .ok_or("account not found")?;
    // A project profile holding this account hands its (possibly newer) login over first, so
    // the account is never signed in at two places at once (see "Project profiles" below).
    if let (Some(vault), Some(root)) = (vault_dir(), profiles_root()) {
        if let Some(newer) = reclaim_profile(&vault, &root, &target) {
            target = newer;
        }
    }
    let mut written = 0;
    for source in sources.iter().filter(|s| installed(provider, s)) {
        write_login(&target, source).map_err(|e| format!("{}: {e}", source.label))?;
        written += 1;
    }
    if written == 0 {
        if let Some(source) = sources.iter().find(|s| s.distro.is_none()).filter(|_| unused_cli) {
            let dir = match provider {
                Provider::Claude => source.claude_dir(),
                Provider::Codex => source.codex_dir(),
            };
            std::fs::create_dir_all(&dir).and_then(|_| write_login(&target, source)).map_err(|e| format!("{}: {e}", source.label))?;
            written += 1;
        }
    }
    if written == 0 {
        return Err("CLI not found".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Project profiles
//
// A folder can be pinned to an account (see `pins`): its CLI then runs with CLAUDE_CONFIG_DIR or
// CODEX_HOME set to that account's profile, a config dir under the app data folder that holds the
// account's saved login.
//
// Both CLIs rotate their refresh token when they renew a login, and the old one stops working, so
// a login copied into two live places breaks whichever copy renews second. The vault avoids this
// for the default location by keeping one account signed in there at a time; profiles keep the
// same rule, an account is signed in at exactly one place:
// - while the CLI's default login is another account, the profile holds the login, and renewals
//   made there are synced back into the vault (the newer login wins, see `keeps`);
// - switching the CLI to the account first takes the profile's login back (`reclaim_profile`) and
//   removes it from the profile. The launcher then runs that folder on the default login, which is
//   the same account, until the CLI is switched away and the profile is signed in again.
// The trade-off: a CLI process that is already running when its login moves keeps the login it
// read in memory, and a renewal made there leaves the other place with a stale login, exactly as
// when switching accounts under a running CLI. Starting the CLI through the launcher picks the
// right place each time.

/// Where profiles live: one folder per pinned account, under the app data folder.
pub fn profiles_root() -> Option<PathBuf> {
    crate::settings::app_dir().map(|d| d.join("profiles"))
}

/// A profile's folder name: short, stable across builds (FNV-1a) and never the account id itself.
pub fn profile_name(provider: Provider, id: &str) -> String {
    let hash = id.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{}-{hash:016x}", provider.key())
}

fn profile_source(dir: &Path) -> Source {
    Source {
        label: "Project profile".into(),
        home: dir.to_owned(),
        claude_dir: Some(dir.to_owned()),
        codex_dir: Some(dir.to_owned()),
        distro: None,
        native_home: dir.to_string_lossy().into_owned(),
    }
}

/// The profiles of these accounts as sources, so their Claude logins' identities can be resolved.
pub fn profile_sources(wanted: &[(Provider, String)]) -> Vec<Source> {
    let Some(root) = profiles_root() else { return vec![] };
    wanted.iter().map(|(p, id)| profile_source(&root.join(profile_name(*p, id)))).collect()
}

/// The saved account signed in to the CLI config dir `dir` (a project profile or any other
/// CLAUDE_CONFIG_DIR / CODEX_HOME), when it is one SlopBar knows. Reads the dir; saves nothing.
pub fn login_at(provider: Provider, dir: &Path) -> Option<String> {
    let known = load_slots();
    let slot = read_login(provider, &profile_source(dir), &known)?;
    known.iter().any(|k| k.provider == provider && k.id == slot.id).then_some(slot.id)
}

/// The file in a profile that holds its login.
pub fn profile_login_file(provider: Provider, dir: &Path) -> PathBuf {
    dir.join(match provider {
        Provider::Claude => ".credentials.json",
        Provider::Codex => "auth.json",
    })
}

/// When a login was issued, to tell two copies of one account's login apart: Claude records its
/// access token's expiry, Codex when it last renewed.
fn issued(slot: &Slot) -> Option<i64> {
    match slot.provider {
        Provider::Claude => slot.data["credentials"]["claudeAiOauth"]["expiresAt"].as_i64(),
        Provider::Codex => slot.data["auth"]["last_refresh"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis()),
    }
}

fn refresh_token(slot: &Slot) -> Option<&str> {
    match slot.provider {
        Provider::Claude => slot.data["credentials"]["claudeAiOauth"]["refreshToken"].as_str(),
        Provider::Codex => slot.data["auth"]["tokens"]["refresh_token"].as_str(),
    }
}

/// How much later the saved login's sign-in must end than a profile's before the saved one counts
/// as a newer sign-in whatever was issued when (see `keeps`): `RENEW_DAYS` plus `EARLY_DAYS`. Along
/// one sign-in's renewals the end stays put or moves a little earlier (the provider may end one up
/// to `EARLY_DAYS` early), while "Sign in all accounts" makes a new sign-in at most `RENEW_DAYS`
/// before the old one ends, and it ends about 30 days after it is made: at least 20 days later.
const NEWER_SIGN_IN_DAYS: i64 = crate::monitoring::RENEW_DAYS + crate::monitoring::EARLY_DAYS;

/// Whether a profile's `live` login should replace the vault's `saved` copy of the same account:
/// it differs and the saved one is not certainly newer. The profile is where the account was in
/// use, so it wins when the order is unknown. The saved one is certainly newer when it was issued
/// later, or when it is a new sign-in (`store_renewal`) whose end is weeks after the live one's: a
/// CLI in the project that renewed the sign-in it replaced before the next sync wrote the new one
/// there holds a copy issued later, which would otherwise win and undo the new sign-in. (After that
/// sync the CLI takes up the new one: Claude Code 2.1.330 re-reads its saved sign-in under its own
/// lock before each renewal and adopts one that changed.) Only the saved copy wins by its end, and
/// only by that margin, so a newer copy of one sign-in whose end moved earlier is never taken for
/// the older one: writing an older copy over a newer one would leave a refresh token already
/// rotated away.
fn keeps(live: &Slot, saved: &Slot) -> bool {
    let saved_newer = matches!((issued(saved), issued(live)), (Some(s), Some(l)) if s > l)
        || matches!((saved.ends_at(), live.ends_at()), (Some(s), Some(l)) if s - l > chrono::Duration::days(NEWER_SIGN_IN_DAYS));
    refresh_token(live) != refresh_token(saved) && !saved_newer
}

/// Signs a profile in as `slot`, creating what a fresh config dir needs.
fn write_profile(slot: &Slot, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    match slot.provider {
        Provider::Claude => {
            // `write_login` only replaces the account block of an existing .claude.json.
            let config = dir.join(".claude.json");
            if !config.exists() {
                write_json(&config, &json!({ "hasCompletedOnboarding": true }))?;
            }
        }
        Provider::Codex => {
            // Keep the profile's login in its own auth.json, out of the shared credential store.
            let config = dir.join("config.toml");
            if !config.exists() {
                std::fs::write(config, "cli_auth_credentials_store = \"file\"\n")?;
            }
        }
    }
    write_login(slot, &profile_source(dir))
}

/// Removes a profile's login, keeping the rest of the folder (the CLI's settings and history).
pub fn withdraw_profile(provider: Provider, dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(profile_login_file(provider, dir)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Takes `target`'s login back from its profile before the default location is signed in as it.
/// Returns the profile's login when that one should be used instead (saved into the vault), and
/// leaves the profile without a login. Callers hold `VAULT`.
fn reclaim_profile(vault: &Path, root: &Path, target: &Slot) -> Option<Slot> {
    let dir = root.join(profile_name(target.provider, &target.id));
    let live = read_login(target.provider, &profile_source(&dir), std::slice::from_ref(target))
        .filter(|l| l.id == target.id && keeps(l, target));
    if let Some(l) = &live {
        let _ = save_slot_in(vault, l);
    }
    let _ = withdraw_profile(target.provider, &dir);
    live
}

/// Where a pinned account's profile stands after `sync_profiles`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileState {
    /// The profile is signed in as the account.
    Ready,
    /// The account is the CLI's default login, so the profile is empty and the folder uses that.
    AtDefault,
    /// SlopBar has no saved login for the account.
    NoLogin,
}

/// Keeps each pinned account's profile signed in, or empty while the account is the default
/// login somewhere (`active`), syncing renewals made in profiles back into the vault. Profiles
/// no longer pinned lose their login.
pub fn sync_profiles(wanted: &[(Provider, String)], active: &Active) -> HashMap<(Provider, String), ProfileState> {
    let (Some(vault), Some(root)) = (vault_dir(), profiles_root()) else { return HashMap::new() };
    if wanted.is_empty() && !root.exists() {
        return HashMap::new();
    }
    let _guard = VAULT.lock().unwrap();
    sync_profiles_in(&vault, &root, wanted, active)
}

fn sync_profiles_in(vault: &Path, root: &Path, wanted: &[(Provider, String)], active: &Active) -> HashMap<(Provider, String), ProfileState> {
    let slots = load_slots_in(vault);
    let saved = |provider: Provider, id: &str| slots.iter().find(|s| s.provider == provider && s.id == id);
    let mut states = HashMap::new();
    for (provider, id) in wanted {
        let key = (*provider, id.clone());
        if states.contains_key(&key) {
            continue;
        }
        let dir = root.join(profile_name(*provider, id));
        let live = read_login(*provider, &profile_source(&dir), &slots);
        let mut current = saved(*provider, id).cloned();
        match &live {
            Some(l) if l.id == *id => {
                if current.as_ref().map_or(true, |s| keeps(l, s)) && save_slot_in(vault, l).is_ok() {
                    current = Some(l.clone());
                }
            }
            // Someone signed in to another account here: keep it, like any login SlopBar sees.
            Some(l) => { let _ = save_slot_in(vault, l); }
            None => {}
        }
        let state = if active.contains_key(&key) {
            let _ = withdraw_profile(*provider, &dir);
            ProfileState::AtDefault
        } else if let Some(slot) = current {
            let signed_in = live.as_ref().is_some_and(|l| l.id == *id && refresh_token(l) == refresh_token(&slot));
            if signed_in || write_profile(&slot, &dir).is_ok() { ProfileState::Ready } else { ProfileState::NoLogin }
        } else {
            ProfileState::NoLogin
        };
        states.insert(key, state);
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(provider) = Provider::ALL.into_iter().find(|p| name.starts_with(&format!("{}-", p.key()))) else { continue };
            if wanted.iter().any(|(p, id)| profile_name(*p, id) == name) {
                continue;
            }
            let dir = entry.path();
            if let Some(l) = read_login(provider, &profile_source(&dir), &slots) {
                // Only into an account the vault still has: a removed account stays removed.
                if saved(provider, &l.id).is_some_and(|s| keeps(&l, s)) {
                    let _ = save_slot_in(vault, &l);
                }
            }
            let _ = withdraw_profile(provider, &dir);
        }
    }
    states
}

// ---------------------------------------------------------------------------------------------
// Adding an account

/// Whether the CLI can actually be run at `source`: `Some(true)` on PATH (in WSL, its login
/// shell's), `Some(false)` only in an installer's folder or as the Claude desktop app's own copy,
/// None not at all. A config dir alone isn't enough: editor extensions and desktop apps create
/// `~/.codex` or `~/.claude` without installing the CLI. On Windows this is `cli::find` (the
/// sign-in runs the one it finds, see `login::command`).
fn cli_at(provider: Provider, source: &Source) -> Option<bool> {
    match &source.distro {
        // A login shell, so ~/.local/bin and version managers are on PATH.
        // With a deadline, so a hung WSL cannot hold up adding an account.
        Some(distro) => sources::wsl(&["-d", distro, "--", "bash", "-lc", &format!("command -v {}", crate::cli::name(provider))], std::time::Duration::from_secs(15))
            .is_some_and(|o| o.status.success())
            .then_some(true),
        None => crate::cli::find(provider).map(|f| f.kind == crate::cli::Kind::Path),
    }
}

/// Where to run a sign-in: a source that has the CLI, one where it is on PATH (or in WSL) before
/// one with it only in an installer's folder or as the Claude desktop app's own copy, so a WSL
/// user signs in there as before; within each, one where it's signed in first.
fn login_source(provider: Provider, sources: &[Source]) -> Option<&Source> {
    let with_cli: Vec<(&Source, bool)> = sources.iter().filter_map(|s| Some((s, cli_at(provider, s)?))).collect();
    pick_login_source(&with_cli, |s| read_login(provider, s, &[]).is_some())
}

/// `login_source`'s choice among sources with the CLI (`true`: on PATH or in WSL).
fn pick_login_source<'a>(with_cli: &[(&'a Source, bool)], signed_in: impl Fn(&Source) -> bool) -> Option<&'a Source> {
    [true, false].into_iter().find_map(|on_path| {
        let tier: Vec<&'a Source> = with_cli.iter().filter(|(_, p)| *p == on_path).map(|(s, _)| *s).collect();
        tier.iter().find(|s| signed_in(s)).or_else(|| tier.first()).copied()
    })
}

/// Where a sign-in's page opens and what it prefills (see `signin`). Both apply to sign-ins run on
/// Windows; in WSL the CLI opens its page as it always did.
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    /// The account expected to sign in: prefilled where the CLI can (Claude's `--email`).
    pub email: Option<String>,
    /// None: the CLI opens the default browser itself. Some is asked for, not promised: where the
    /// page really opens is the started sign-in's to say (`LoginJob::opens_in`).
    pub target: Option<crate::signin::Target>,
}

/// Opens a terminal that runs the CLI's own sign-in in an isolated temporary config dir, then
/// imports the account once the sign-in finishes. The current login is never touched.
pub fn start_login_observed(provider: Provider, sources: &[Source], diagnostics: Option<crate::diagnostics::Context>, options: LoginOptions) -> Result<LoginJob, String> {
    use crate::diagnostics::LoginStage;
    let source = login_source(provider, sources);
    if let Some(d) = &diagnostics { d.login(LoginStage::CliDiscovery, source.is_some(), source.is_some_and(|s| s.distro.is_some()), None, 0); }
    let source = source.ok_or("CLI not found")?.clone();
    // A relayed sign-in's page must reach the computer completing it, which only a Windows CLI here
    // hands over: in WSL the CLI opens its own browser.
    let relayed = matches!(options.target, Some(crate::signin::Target::Relay(_)));
    if relayed && source.distro.is_some() {
        return Err("relay_unsupported".into());
    }
    let name = login_dir_name();
    let target = options.target.filter(|_| source.distro.is_none());
    let mut cmd = crate::login::action(provider, false).to_owned();
    if provider == Provider::Claude {
        // Checked to plain address characters: it is written into the sign-in's cmd script.
        if let Some(email) = crate::signin::prefill_email(options.email.as_deref()) {
            cmd.push_str(" --email ");
            cmd.push_str(email);
        }
    }
    let cmd = cmd.as_str();
    let dir = source.home.join(&name);
    // Never reuse a directory (or a symlink) left by an earlier/crashed sign-in.
    let made = std::fs::create_dir(&dir);
    if let Some(d) = &diagnostics { d.login(LoginStage::Directory, made.is_ok(), source.distro.is_some(), None, 0); }
    made.map_err(|_| "monitor_storage".to_string())?;
    // Keep an isolated Codex login out of the machine's shared credential-store entry.
    // A config file also avoids different cmd.exe / bash quoting rules for TOML strings.
    if provider == Provider::Codex
        && std::fs::write(dir.join("config.toml"), "cli_auth_credentials_store = \"file\"\n").is_err() {
        if let Some(d) = &diagnostics { d.login(LoginStage::Prepare, false, source.distro.is_some(), None, 0); }
        let _ = std::fs::remove_dir_all(&dir);
        return Err("monitor_storage".into());
    }

    let prepared = crate::login::prepare(&dir, provider, cmd);
    if let Some(d) = &diagnostics { d.login(LoginStage::Prepare, prepared.is_ok(), source.distro.is_some(), None, 0); }
    if prepared.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("monitor_storage".into());
    }
    let native = format!("{}/{name}", source.native_home);
    let login = Source {
        label: source.label.clone(),
        home: dir.clone(),
        claude_dir: Some(dir.clone()),
        codex_dir: Some(dir.clone()),
        distro: source.distro.clone(),
        native_home: source.native_home.clone(),
    };
    if let (Provider::Codex, Some(target)) = (provider, &target) {
        // The page in the chosen window, through the app server; an older Codex without one
        // falls back to `codex login` and the default browser.
        if let Ok((child, tree, rpc)) = start_codex_app(&source, &dir, &native, target) {
            if let Some(d) = &diagnostics { d.login(LoginStage::ProcessStart, true, false, None, 0); }
            return Ok(LoginJob { provider, source: login, dir, native, started: Instant::now(), child, tree: Some(tree), rpc: Some(rpc), again: None, diagnostics, finished: false,
                opens_in: Some(target.clone()) });
        }
        // `codex login` would open this computer's own browser, where nobody completes it.
        if relayed {
            if let Some(d) = &diagnostics { d.login(LoginStage::ProcessStart, false, false, None, 0); }
            let _ = std::fs::remove_dir_all(&dir);
            return Err("relay_unsupported".into());
        }
    }
    let visible = console(target.as_ref());
    let mut start = CliStart { provider, source: source.clone(), cmd: cmd.to_owned(), visible, env: None, breakaway: false, again: 0 };
    if let (Provider::Claude, Some(target)) = (provider, &target) {
        if let Some(env) = crate::signin::claude_browser_env(target) {
            start.env = Some(env);
            // The browser it opens outlives the sign-in; a relayed page opens no browser here.
            start.breakaway = !relayed;
        } else if relayed {
            // Without `BROWSER` the CLI would open this computer's own browser, where nobody is.
            let _ = std::fs::remove_dir_all(&dir);
            return Err("relay_unsupported".into());
        }
    }
    let (child, tree) = match start.spawn(&dir, &native) {
        Ok(process) => process,
        Err(_) => {
            if let Some(d) = &diagnostics { d.login(LoginStage::ProcessStart, false, source.distro.is_some(), None, 0); }
            let _ = std::fs::remove_dir_all(&dir);
            return Err("monitor_login_failed".into());
        }
    };
    if let Some(d) = &diagnostics { d.login(LoginStage::ProcessStart, true, source.distro.is_some(), None, 0); }
    // The page opens in the target only through `BROWSER`; without it (`codex login`, which
    // ignores it, a CLI in WSL, which is given no target, or no `BROWSER` to give) the CLI opens
    // its own browser.
    let opens_in = start.env.as_ref().and(target);
    // Only a page handed to SlopBar (`BROWSER`) can be refused and the CLI started again.
    let again = start.env.is_some().then_some(start);
    Ok(LoginJob { provider, source: login, dir, native, started: Instant::now(), child, tree: Some(tree), rpc: None, again, diagnostics, finished: false, opens_in })
}

/// A new sign-in folder's name, `.slopbar-login-<process>-<millis>-<n>`: startup cleanup knows it
/// by that (`login::abandoned`) once it is old.
fn login_dir_name() -> String {
    static LOGIN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(".slopbar-login-{}-{}-{}", std::process::id(), chrono::Utc::now().timestamp_millis(), LOGIN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// How many times, at most, a sign-in's CLI is started when each page it makes names a callback
/// port browsers refuse (see `signin::PAGE_REFUSED`): Windows hands out free ports in order, so a
/// new CLI often gets the next one, and this steps past the longest run of refused ones (6665-6669)
/// with room. A relayed sign-in starts its whole sign-in again as often (see `relay`).
pub const REFUSED_TRIES: u32 = 8;

/// How a CLI sign-in's process starts, kept so the same sign-in can start again in its folder when
/// its page named a port browsers refuse: the CLI there listens on another port the next time.
struct CliStart {
    provider: Provider,
    /// Where the CLI is found (the sign-in folder's own `Source` is the job's).
    source: Source,
    cmd: String,
    visible: bool,
    /// `BROWSER` and where it opens the page (see `signin::claude_browser_env`).
    env: Option<[(std::ffi::OsString, std::ffi::OsString); 2]>,
    breakaway: bool,
    /// How many times it was started again (the first start not counted).
    again: u32,
}

impl CliStart {
    fn spawn(&self, dir: &Path, native: &str) -> std::io::Result<(std::process::Child, crate::login::ProcessTree)> {
        let mut term = crate::login::command(self.provider, &self.source, dir, native, &self.cmd, self.visible);
        if !self.visible {
            term.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        }
        if let Some(env) = &self.env {
            term.envs(env.iter().map(|(name, value)| (name, value)));
        }
        crate::login::ProcessTree::spawn_with(&mut term, self.visible, self.breakaway)
    }
}

/// Whether a CLI sign-in gets a console window of its own: it does (its terminal says what to do
/// when the page does not open), except a relayed one, which runs where nobody looks and hands its
/// page to the computer completing it (see `relay`).
fn console(target: Option<&crate::signin::Target>) -> bool {
    !matches!(target, Some(crate::signin::Target::Relay(_)))
}

/// Starts Codex's app server hidden in the sign-in folder and opens its sign-in page in `target`.
fn start_codex_app(source: &Source, dir: &Path, native: &str, target: &crate::signin::Target)
    -> Result<(std::process::Child, crate::login::ProcessTree, crate::codex_signin::Started), String> {
    use std::process::Stdio;
    let mut command = crate::login::command(Provider::Codex, source, dir, native, crate::codex_signin::ACTION, false);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let (mut child, tree) = crate::login::ProcessTree::spawn(&mut command, false).map_err(|_| "monitor_login_failed")?;
    let pipes = child.stdin.take().zip(child.stdout.take());
    let started = pipes.ok_or_else(|| "monitor_login_failed".to_string())
        .and_then(|(stdin, stdout)| crate::codex_signin::start(stdin, stdout, |url| crate::signin::open(url, target)));
    match started {
        Ok(rpc) => Ok((child, tree, rpc)),
        Err(e) => {
            drop(tree);
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

pub struct LoginJob {
    diagnostics: Option<crate::diagnostics::Context>,
    finished: bool,
    tree: Option<crate::login::ProcessTree>,
    /// A Codex sign-in through the app server (see `codex_signin`): it says when the user is done
    /// instead of exiting.
    rpc: Option<crate::codex_signin::Started>,
    /// How to start the CLI again when its page named a port browsers refuse (see `CliStart`).
    again: Option<CliStart>,
    native: String,
    child: std::process::Child,
    provider: Provider,
    source: Source,
    dir: PathBuf,
    started: Instant,
    /// Where its page opens (see `opens_in`).
    opens_in: Option<crate::signin::Target>,
}

impl LoginJob {
    /// Where its page opens: the target it was given when its CLI hands the page to SlopBar there
    /// (Codex's app server, or Claude Code's `BROWSER`), so it also does when started again; None
    /// when the CLI opens its own browser, whatever window was asked for (a CLI found only in WSL,
    /// a Codex that fell back to `codex login`, no `BROWSER` to give). A run's watches follow this,
    /// never the window it asked for (see `app::Placed`).
    pub fn opens_in(&self) -> Option<&crate::signin::Target> {
        self.opens_in.as_ref()
    }

    /// Polls until the sign-in has produced a login, then imports it and cleans up.
    /// Gives up after 15 minutes (the temp dir is removed either way).
    pub async fn wait(self, client: &reqwest::Client) -> Option<Slot> {
        let slot = self.wait_isolated(client).await.ok()?;
        let _guard = VAULT.lock().unwrap();
        save_slot(&slot).ok()?;
        Some(slot)
    }

    /// Complete a separate login without ever importing it into the switching vault. Fails with
    /// `signin_port_refused` when every page its CLI made named a callback port browsers refuse
    /// (see [`REFUSED_TRIES`]), else `monitor_login_failed`.
    pub async fn wait_isolated(mut self, client: &reqwest::Client) -> Result<Slot, &'static str> {
        use crate::diagnostics::LoginStage;
        const FAILED: &str = "monitor_login_failed";
        let result = loop {
            if self.started.elapsed() > Duration::from_secs(15 * 60) {
                self.record_login(LoginStage::Timeout, false, None);
                break Err(FAILED);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Some(rpc) = &self.rpc {
                // The app server reports the end itself and stays running.
                match rpc.done.try_recv() {
                    Ok(true) => self.record_login(LoginStage::ProcessExit, true, None),
                    Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                    _ => { self.record_login(LoginStage::ProcessExit, false, None); break Err(FAILED); }
                }
                let mut slot = None;
                for _ in 0..10 {
                    slot = read_login(self.provider, &self.source, &[]);
                    if slot.is_some() { break; }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                self.record_login(LoginStage::Credentials, slot.is_some(), None);
                break slot.ok_or(FAILED);
            }
            // Its page named a callback port browsers refuse, so it was not opened: the CLI starts
            // again and listens on another port. (One that finished meanwhile is read as usual.)
            if self.page_refused() && !matches!(self.child.try_wait(), Ok(Some(status)) if status.success()) {
                match self.start_again() {
                    Ok(()) => continue,
                    Err(e) => break Err(e),
                }
            }
            // Wait for successful CLI exit, not the first partially written credential file.
            match self.child.try_wait() {
                Ok(Some(status)) => { self.record_login(LoginStage::ProcessExit, status.success(), status.code()); if !status.success() { break Err(FAILED); } },
                Ok(None) => continue,
                _ => { self.record_login(LoginStage::ProcessExit, false, None); break Err(FAILED); },
            }
            let slot = read_login(self.provider, &self.source, &[]);
            self.record_login(LoginStage::Credentials, slot.is_some(), None);
            let Some(mut slot) = slot else { break Err(FAILED) };
            if slot.provider == Provider::Claude {
                // A monitor must be bound to the server-verified account, not a config label.
                let verified = with_claude_profile(client, slot).await;
                self.record_login(LoginStage::Profile, verified.is_some(), None);
                let Some(verified) = verified else { break Err(FAILED) };
                slot = verified;
            }
            break Ok(slot);
        };
        // Close the process tree before reading/removing any remaining login state.
        drop(self.rpc.take());
        drop(self.tree.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Credentials may reach the vault, main CLI, or monitor only after a
        // durable handoff marker prevents later cleanup from logging them out.
        let result = result.and_then(|slot| {
            let ok = crate::login::handoff(&self.dir, self.provider).is_ok();
            self.record_login(LoginStage::Handoff, ok, None);
            if ok { Ok(slot) } else { Err(FAILED) }
        });
        if result.is_err() {
            let (provider, source, dir, native) = (self.provider, self.source.clone(), self.dir.clone(), self.native.clone());
            let _ = tokio::task::spawn_blocking(move || crate::login::logout(provider, &source, &dir, &native)).await;
        }
        let removed = std::fs::remove_dir_all(&self.dir);
        self.record_login(LoginStage::FolderCleanup, removed.is_ok(), None);
        self.finished = true;
        result
    }

    /// Whether the CLI's page was refused for its callback port (see `signin::PAGE_REFUSED`).
    fn page_refused(&self) -> bool {
        self.again.is_some() && std::fs::symlink_metadata(self.dir.join(crate::signin::PAGE_REFUSED)).is_ok()
    }

    /// Ends the CLI, and everything it started, and starts it again in a new sign-in folder, so it
    /// listens on another port: up to [`REFUSED_TRIES`] starts in all. The old folder goes once its
    /// processes let go of it; its page never opened, so it holds no sign-in. (Each start shows in
    /// diagnostics as a `ProcessStart` of its own.)
    fn start_again(&mut self) -> Result<(), &'static str> {
        // The old CLI goes first, and everything it started: its port with it.
        drop(self.tree.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        let Some(start) = self.again.as_mut() else { return Err("monitor_login_failed") };
        if start.again + 1 >= REFUSED_TRIES {
            return Err("signin_port_refused");
        }
        start.again += 1;
        // A new folder: the old CLI's processes may still be ending, and writing to theirs. Never one
        // that was there already (as at the first start): only a folder made here is removed.
        let name = login_dir_name();
        let dir = start.source.home.join(&name);
        if std::fs::create_dir(&dir).is_err() {
            return Err("monitor_storage");
        }
        if crate::login::prepare(&dir, start.provider, &start.cmd).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err("monitor_storage");
        }
        let old = std::mem::replace(&mut self.dir, dir.clone());
        self.native = format!("{}/{name}", start.source.native_home);
        self.source = Source { home: dir.clone(), claude_dir: Some(dir.clone()), codex_dir: Some(dir), ..self.source.clone() };
        remove_when_free(old);
        let started = start.spawn(&self.dir, &self.native);
        let ok = started.is_ok();
        if let Ok((child, tree)) = started {
            self.child = child;
            self.tree = Some(tree);
        }
        self.record_login(crate::diagnostics::LoginStage::ProcessStart, ok, None);
        if ok { Ok(()) } else { Err("monitor_login_failed") }
    }

    fn record_login(&self, stage: crate::diagnostics::LoginStage, ok: bool, exit_code: Option<i32>) {
        if let Some(d) = &self.diagnostics { d.login(stage, ok, self.source.distro.is_some(), exit_code, crate::diagnostics::millis(self.started)); }
    }
}

/// Deletes a sign-in folder whose processes are ending, retrying while they still hold it; one
/// still held after that is left to startup cleanup.
fn remove_when_free(dir: PathBuf) {
    std::thread::spawn(move || {
        for _ in 0..20 {
            if std::fs::remove_dir_all(&dir).is_ok() || std::fs::symlink_metadata(&dir).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

impl Drop for LoginJob {
    fn drop(&mut self) {
        if !self.finished { self.record_login(crate::diagnostics::LoginStage::Cancelled, false, None); }
        drop(self.rpc.take());
        drop(self.tree.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.finished {
            return;
        }
        // Stopped or started again before it finished: a sign-in its CLI may have made meanwhile
        // was never handed over (that happens only as it finishes), so it is signed out and the
        // folder removed now, as a failed one is, rather than left until SlopBar next starts.
        let (provider, source, dir, native) = (self.provider, self.source.clone(), self.dir.clone(), self.native.clone());
        std::thread::spawn(move || {
            // Its processes, ended above, let go of the folder first.
            std::thread::sleep(Duration::from_secs(1));
            // Never a login that was handed over (nor one whose folder says nothing readable):
            // startup cleanup's own rule.
            if !crate::login::not_handed_off(&dir) {
                return;
            }
            crate::login::logout(provider, &source, &dir, &native);
            for _ in 0..10 {
                if std::fs::remove_dir_all(&dir).is_ok() || std::fs::symlink_metadata(&dir).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }
}

/// A Claude account's identity, in `.claude.json`'s `oauthAccount` shape.
async fn claude_profile(client: &reqwest::Client, token: &str) -> Option<Value> {
    let p: Value = client
        .get("https://api.anthropic.com/api/oauth/profile")
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    p["account"]["uuid"].as_str()?;
    Some(json!({
        "accountUuid": p["account"]["uuid"],
        "emailAddress": p["account"]["email"],
        "organizationUuid": p["organization"]["uuid"],
        "organizationName": p["organization"]["name"],
        "displayName": p["account"]["display_name"],
    }))
}

/// Replaces a new Claude login's identity with the profile API's answer.
async fn with_claude_profile(client: &reqwest::Client, slot: Slot) -> Option<Slot> {
    let (token, _) = slot.access()?;
    let account = claude_profile(client, &token).await?;
    claude_slot(slot.data["credentials"].clone(), Some(account))
}

#[cfg(test)]
mod billing_tests {
    use super::*;

    #[test]
    fn billing_hints_come_from_the_saved_login_without_identities() {
        let claims = json!({ "email": "test@example.invalid", "https://api.openai.com/auth": {
            "chatgpt_plan_type": "plus", "chatgpt_account_id": "acct",
            "chatgpt_subscription_active_start": "2026-09-18T21:26:13+00:00",
            "chatgpt_subscription_active_until": "2026-10-18T21:26:13+00:00",
            "chatgpt_subscription_last_checked": "2026-10-09T16:01:36.550331+00:00",
        } });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        let codex = codex_slot(json!({ "tokens": { "access_token": "a", "id_token": format!("h.{payload}.s") } })).unwrap();
        let b = codex.billing();
        assert_eq!(b.active_until.unwrap().to_rfc3339(), "2026-10-18T21:26:13+00:00");
        assert!(b.started_at.is_some() && b.checked_at.is_some() && b.tier.is_none());

        let claude = claude_slot(json!({ "claudeAiOauth": { "accessToken": "a", "subscriptionType": "max", "rateLimitTier": "default_claude_max_5x" } }),
            Some(json!({ "accountUuid": "u", "emailAddress": "test@example.invalid", "billingType": "stripe_subscription",
                "subscriptionCreatedAt": "2026-08-03T09:00:00.000000Z" }))).unwrap();
        let b = claude.billing();
        assert_eq!((b.tier.as_deref(), b.billing_type.as_deref()), (Some("default_claude_max_5x"), Some("stripe_subscription")));
        assert_eq!(b.started_at.unwrap().to_rfc3339(), "2026-08-03T09:00:00+00:00");
        let text = serde_json::to_string(&b).unwrap();
        assert!(!text.contains("example.invalid") && !text.contains("\"a\""), "{text}");
        assert!(codex_slot(json!({ "tokens": { "access_token": "a" } })).unwrap().billing().active_until.is_none());
    }
}

#[cfg(test)]
mod login_source_tests {
    use super::*;

    fn source(label: &str) -> Source {
        Source { label: label.into(), home: PathBuf::from(label), claude_dir: None, codex_dir: None, distro: None, native_home: label.into() }
    }

    /// A CLI on PATH (or in WSL) is where sign-ins ran before the installers' folders and the
    /// Claude desktop app's copy counted, so it still wins over them, signed in or not; within each,
    /// a source where the CLI is signed in comes first.
    #[test]
    fn the_cli_on_path_wins_and_then_the_one_signed_in() {
        let (windows, wsl, other) = (source("Windows"), source("WSL: Ubuntu"), source("WSL: Debian"));
        let label = |s: Option<&Source>| s.map(|s| s.label.clone());
        let windows_signed_in = |s: &Source| s.label == "Windows";
        // The desktop app's copy on a signed-in Windows, the CLI in WSL: WSL, as before.
        assert_eq!(label(pick_login_source(&[(&windows, false), (&wsl, true)], windows_signed_in)), Some("WSL: Ubuntu".into()));
        // Both on PATH: the signed-in one, else the first.
        assert_eq!(label(pick_login_source(&[(&wsl, true), (&windows, true)], windows_signed_in)), Some("Windows".into()));
        assert_eq!(label(pick_login_source(&[(&wsl, true), (&other, true)], |_| false)), Some("WSL: Ubuntu".into()));
        // Only the new places: the signed-in one, else the first; none at all: none.
        assert_eq!(label(pick_login_source(&[(&other, false), (&windows, false)], windows_signed_in)), Some("Windows".into()));
        assert_eq!(label(pick_login_source(&[(&other, false)], |_| false)), Some("WSL: Debian".into()));
        assert_eq!(label(pick_login_source(&[], |_| true)), None);
    }

    /// A sign-in another computer completes (see `relay`) runs where nobody looks: no console
    /// window of its own, and it never falls back to the CLI opening a browser here (`codex login`,
    /// a CLI in WSL), which would open a page on this computer's screen.
    #[test]
    fn a_relayed_sign_in_gets_no_window_and_never_falls_back_to_a_browser_here() {
        use crate::signin::Target;
        assert!(console(None) && console(Some(&Target::Private)), "a sign-in here keeps its terminal");
        assert!(!console(Some(&Target::Relay("00".into()))));
        let code = crate::source_guard::tokens(include_str!("accounts.rs"));
        let start = crate::source_guard::function(&code, "accounts.rs", "pub fn start_login_observed(", "accounts::login_source_tests (src-tauri/src/accounts.rs)");
        for (needle, why) in [
            ("if relayed && source.distro.is_some() { return Err(\"relay_unsupported\".into()); }", "a CLI in WSL opens its own browser"),
            ("if relayed { if let Some(d) = &diagnostics { d.login(LoginStage::ProcessStart, false, false, None, 0); } let _ = std::fs::remove_dir_all(&dir); return Err(\"relay_unsupported\".into()); }",
                "codex login opens this computer's browser"),
            ("let visible = console(target.as_ref()); let mut start = CliStart { provider, source: source.clone(), cmd: cmd.to_owned(), visible,",
                "the terminal follows `console`"),
            ("start.breakaway = !relayed;", "a relayed page opens no browser here to outlive the sign-in"),
            ("} else if relayed { let _ = std::fs::remove_dir_all(&dir); return Err(\"relay_unsupported\".into()); }", "without BROWSER the CLI opens a browser here"),
            ("let (child, tree) = match start.spawn(&dir, &native) {", "the first start is the one a start again repeats"),
        ] {
            assert!(crate::source_guard::has(start, &crate::source_guard::tokens(needle)), "{why}: {needle}\n{start}");
        }
        // Every start, the first and each one again, runs the same command the same way.
        let spawn = crate::source_guard::function(&code, "accounts.rs", "fn spawn(&self, dir: &Path, native: &str)", "accounts::login_source_tests (src-tauri/src/accounts.rs)");
        for needle in ["let mut term = crate::login::command(self.provider, &self.source, dir, native, &self.cmd, self.visible);",
            "if !self.visible { term.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()); }",
            "crate::login::ProcessTree::spawn_with(&mut term, self.visible, self.breakaway)"] {
            assert!(crate::source_guard::has(spawn, &crate::source_guard::tokens(needle)), "{needle}\n{spawn}");
        }
    }

    /// A started sign-in says where its page really opens (`LoginJob::opens_in`), whatever window it
    /// was asked for: in that target only when its CLI hands the page to SlopBar there (Codex's app
    /// server, or Claude Code given `BROWSER`), and so again when it starts again; never through a
    /// CLI found only in WSL, which is given no target and opens its own browser, nor through
    /// `codex login`, which ignores `BROWSER`. A run watches the email's profile by what this says,
    /// never by what it asked for (see `app::Placed`): a run that did stopped with `signin_no_page`
    /// after 40 s while the page waited in the default browser.
    #[test]
    fn a_started_sign_in_says_where_its_page_really_opens() {
        use crate::source_guard::{count, function, has, tokens};
        const T: &str = "accounts::login_source_tests::a_started_sign_in_says_where_its_page_really_opens (src-tauri/src/accounts.rs)";
        let code = tokens(include_str!("accounts.rs"));
        let start = function(&code, "accounts.rs", "pub fn start_login_observed(", T);
        assert_eq!(count(start, "LoginJob {"), 2, "two ways a sign-in starts: Codex's app server, and the CLI itself\n{start}");
        assert_eq!(count(start, "opens_in"), 3, "each says where its page opens, once\n{start}");
        for (needle, why) in [
            ("let target = options.target.filter(|_| source.distro.is_none());", "a CLI in WSL is given no target"),
            ("return Ok(LoginJob { provider, source: login, dir, native, started: Instant::now(), child, tree: Some(tree), rpc: Some(rpc), again: None, diagnostics, finished: false, opens_in: Some(target.clone()) });",
                "Codex's app server hands its page to SlopBar, which opens it in the target"),
            ("let opens_in = start.env.as_ref().and(target); let again = start.env.is_some().then_some(start);",
                "the CLI itself opens its page in the target only through `BROWSER`"),
            ("Ok(LoginJob { provider, source: login, dir, native, started: Instant::now(), child, tree: Some(tree), rpc: None, again, diagnostics, finished: false, opens_in })",
                "the CLI's start says so"),
        ] {
            assert!(has(start, needle), "{why}: {needle}\n{start}");
        }
        // Started again for another port, it opens where it did: through the same `BROWSER`.
        let again = function(&code, "accounts.rs", "fn start_again(&mut self)", T);
        assert!(!has(again, "opens_in"), "{again}");
        let said = function(&code, "accounts.rs", "pub fn opens_in(&self)", T);
        assert_eq!(said, tokens("pub fn opens_in(&self) -> Option<&crate::signin::Target> { self.opens_in.as_ref() }"));
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    use crate::test_support::TempDir;

    fn claude(refresh: &str, expires: i64) -> Slot {
        claude_slot(
            json!({ "claudeAiOauth": { "accessToken": format!("at-{refresh}"), "refreshToken": refresh, "expiresAt": expires }, "organizationUuid": "org-1" }),
            Some(json!({ "accountUuid": "user-1", "organizationUuid": "org-1", "emailAddress": "test@example.invalid" })),
        ).unwrap()
    }

    fn codex(refresh: &str, last: &str) -> Slot {
        let claims = json!({ "email": "test@example.invalid", "https://api.openai.com/auth": { "chatgpt_user_id": "user-2", "chatgpt_account_id": "acct-2" } });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        codex_slot(json!({ "last_refresh": last, "tokens": { "access_token": format!("at-{refresh}"), "refresh_token": refresh, "id_token": format!("h.{payload}.s") } })).unwrap()
    }

    fn stored(vault: &Path, slot: &Slot) -> Option<String> {
        load_slots_in(vault).into_iter().find(|s| s.id == slot.id).and_then(|s| refresh_token(&s).map(String::from))
    }

    /// A Claude login that says when its sign-in ends, as Claude Code 2.1.330 saves it.
    fn ending(refresh: &str, expires: i64, ends: i64) -> Slot {
        let mut slot = claude(refresh, expires);
        slot.data["credentials"]["claudeAiOauth"]["refreshTokenExpiresAt"] = json!(ends);
        slot
    }

    /// Milliseconds `days` from now.
    fn in_days(days: i64) -> i64 {
        (chrono::Utc::now() + chrono::Duration::days(days)).timestamp_millis()
    }

    #[test]
    fn a_saved_cli_sign_in_says_when_it_ends_and_whether_it_needs_a_new_one() {
        use crate::model::RenewalKind;
        let now = chrono::Utc::now();
        let at = |days: i64| now + chrono::Duration::days(days);
        let end = at(3).timestamp_millis();
        assert_eq!(ending("r", 1, end).ends_at().map(|t| t.timestamp_millis()), Some(end));
        assert_eq!(claude("r", 1).ends_at(), None, "a login saved by an older Claude Code says nothing");
        assert_eq!(codex("r", "2026-10-01T00:00:00Z").ends_at(), None, "Codex gives no end");
        // Nothing a sign-in could mean marks an account's sign-in as ended.
        for nonsense in [0, 1_700_000_000, in_days(800)] {
            assert_eq!(ending("r", 1, nonsense).ends_at(), None, "{nonsense}");
        }
        assert_eq!(cli_renewal(Some(at(-1)), false, now), Some(RenewalKind::Ended));
        assert_eq!(cli_renewal(Some(now), false, now), Some(RenewalKind::Ended), "it ends at its end");
        assert_eq!(cli_renewal(Some(at(6)), false, now), Some(RenewalKind::Ending), "within RENEW_DAYS");
        assert_eq!(cli_renewal(Some(at(8)), false, now), None);
        assert_eq!(cli_renewal(None, false, now), None);
        assert_eq!(cli_renewal(Some(at(-1)), true, now), None, "the CLI manages the sign-in in use");
    }

    #[test]
    fn a_new_cli_sign_in_replaces_the_saved_one_and_keeps_details_only_the_old_one_had() {
        let vault = TempDir::new();
        let mut old = ending("r-old", 1_000, in_days(-2));
        old.data["oauth_account"]["billingType"] = json!("stripe_subscription");
        old.data["oauth_account"]["emailAddress"] = json!("old@example.invalid");
        save_slot_in(&vault.0, &old).unwrap();
        // Another account's saved login, which nothing here touches.
        let other = claude_slot(json!({ "claudeAiOauth": { "accessToken": "at-x", "refreshToken": "r-x" }, "organizationUuid": "org-1" }),
            Some(json!({ "accountUuid": "user-9", "organizationUuid": "org-1", "billingType": "apple" }))).unwrap();
        save_slot_in(&vault.0, &other).unwrap();

        let end = in_days(30);
        let new = ending("r-new", 2_000, end);
        assert_eq!(store_renewal_in(&vault.0, &new, &[]).unwrap(), Renewed::Saved);
        let saved = load_slots_in(&vault.0).into_iter().find(|s| s.id == new.id).unwrap();
        assert_eq!((refresh_token(&saved), saved.ends_at().map(|t| t.timestamp_millis())), (Some("r-new"), Some(end)));
        assert_eq!(saved.billing().billing_type.as_deref(), Some("stripe_subscription"), "kept: the profile API does not return it");
        assert_eq!(saved.data["oauth_account"]["emailAddress"], "test@example.invalid", "the new login's own fields win");
        assert_eq!(stored(&vault.0, &other).as_deref(), Some("r-x"));
        assert_eq!(load_slots_in(&vault.0).len(), 2);
        // A first sign-in, with nothing saved before it, is saved as it is.
        let fresh = TempDir::new();
        store_login_in(&fresh.0, &new).unwrap();
        assert!(load_slots_in(&fresh.0)[0].data["oauth_account"].get("billingType").is_none());
        // Never another organization's details: the CLI's own record can be one of the same person's.
        let mut elsewhere = old.clone();
        elsewhere.data["oauth_account"]["organizationUuid"] = json!("org-2");
        let mut renewed = ending("r-2", 3_000, end);
        keep_details(&mut renewed, &elsewhere);
        assert!(renewed.data["oauth_account"].get("billingType").is_none());
    }

    #[test]
    fn a_renewal_is_not_saved_once_the_account_is_in_use_or_removed() {
        let (vault, cli) = (TempDir::new(), TempDir::new());
        let old = ending("r-old", 1_000, in_days(-2));
        save_slot_in(&vault.0, &old).unwrap();
        let new = ending("r-new", 2_000, in_days(30));
        // Switched to it while its sign-in was open: the CLI's live login is the old one, which the
        // next capture would put back, so the new one is not saved.
        write_profile(&old, &cli.0).unwrap();
        assert_eq!(store_renewal_in(&vault.0, &new, &[profile_source(&cli.0)]).unwrap(), Renewed::InUse);
        assert_eq!(stored(&vault.0, &old).as_deref(), Some("r-old"));
        // Not in use anywhere: saved.
        assert_eq!(store_renewal_in(&vault.0, &new, &[]).unwrap(), Renewed::Saved);
        assert_eq!(stored(&vault.0, &old).as_deref(), Some("r-new"));
        // Removed while its sign-in was open: it stays removed.
        let empty = TempDir::new();
        assert_eq!(store_renewal_in(&empty.0, &new, &[]).unwrap(), Renewed::Removed);
        assert!(load_slots_in(&empty.0).is_empty());
    }

    #[test]
    fn a_new_sign_in_wins_over_a_project_cli_still_renewing_the_one_it_replaced() {
        let (vault, root) = (TempDir::new(), TempDir::new());
        let old = ending("r-old", 1_000, in_days(2));
        save_slot_in(&vault.0, &old).unwrap();
        let wanted = [(Provider::Claude, old.id.clone())];
        sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
        let dir = root.0.join(profile_name(Provider::Claude, &old.id));
        // Sign in all accounts makes a new sign-in, saved in the vault.
        save_slot_in(&vault.0, &ending("r-new", 2_000, in_days(30))).unwrap();
        // A CLI still running in the project renews the old sign-in after it, and writes it back.
        write_login(&ending("r-old-2", 9_000, in_days(2)), &profile_source(&dir)).unwrap();
        sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
        assert_eq!(stored(&vault.0, &old).as_deref(), Some("r-new"), "the new sign-in stays");
        assert_eq!(in_profile(&root.0, &old).as_deref(), Some("r-new"), "and the project gets it");
        // Its own renewals come back as before.
        write_login(&ending("r-new-2", 10_000, in_days(30)), &profile_source(&dir)).unwrap();
        sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
        assert_eq!(stored(&vault.0, &old).as_deref(), Some("r-new-2"));
        // Switching to the account takes the profile's copy back only when it is newer.
        write_login(&ending("r-old-3", 11_000, in_days(2)), &profile_source(&dir)).unwrap();
        let saved = load_slots_in(&vault.0).into_iter().find(|s| s.id == old.id).unwrap();
        assert!(reclaim_profile(&vault.0, &root.0, &saved).is_none(), "the old sign-in's copy is not taken back");
        assert_eq!(stored(&vault.0, &old).as_deref(), Some("r-new-2"));
    }

    #[test]
    fn a_newer_copy_of_one_sign_in_whose_end_moved_earlier_still_wins() {
        // Along one sign-in's renewals the end can move a few days earlier; the newer copy must win,
        // or the profile and the vault get a refresh token that was already rotated away.
        for days_earlier in [3, 9] {
            let (vault, root) = (TempDir::new(), TempDir::new());
            let saved = ending("r1", 1_000, in_days(20));
            save_slot_in(&vault.0, &saved).unwrap();
            let wanted = [(Provider::Claude, saved.id.clone())];
            sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
            let dir = root.0.join(profile_name(Provider::Claude, &saved.id));
            write_login(&ending("r2", 2_000, in_days(20 - days_earlier)), &profile_source(&dir)).unwrap();
            sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
            assert_eq!(stored(&vault.0, &saved).as_deref(), Some("r2"), "{days_earlier} days earlier, in sync");
            assert_eq!(in_profile(&root.0, &saved).as_deref(), Some("r2"));
            // The same when switching to it takes the profile's copy back.
            write_login(&ending("r3", 3_000, in_days(20 - days_earlier - 1)), &profile_source(&dir)).unwrap();
            let current = load_slots_in(&vault.0).into_iter().find(|s| s.id == saved.id).unwrap();
            assert_eq!(reclaim_profile(&vault.0, &root.0, &current).as_ref().and_then(refresh_token), Some("r3"), "{days_earlier} days earlier, reclaimed");
            assert_eq!(stored(&vault.0, &saved).as_deref(), Some("r3"));
        }
        // One copy without an end (an older Claude Code): the order it was issued in decides, as before.
        let older = claude("r1", 1_000);
        assert!(keeps(&ending("r2", 2_000, in_days(5)), &older) && !keeps(&older, &ending("r2", 2_000, in_days(5))));
    }

    fn in_profile(root: &Path, slot: &Slot) -> Option<String> {
        read_login(slot.provider, &profile_source(&root.join(profile_name(slot.provider, &slot.id))), &[]).and_then(|s| refresh_token(&s).map(String::from))
    }

    #[test]
    fn a_pinned_account_not_in_use_is_signed_in_to_its_profile_and_renewals_come_back() {
        let (vault, root) = (TempDir::new(), TempDir::new());
        let slot = claude("r1", 1_000);
        save_slot_in(&vault.0, &slot).unwrap();
        let wanted = [(Provider::Claude, slot.id.clone())];
        let states = sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
        assert_eq!(states[&wanted[0]], ProfileState::Ready);
        assert_eq!(in_profile(&root.0, &slot).as_deref(), Some("r1"));
        let dir = root.0.join(profile_name(Provider::Claude, &slot.id));
        let config = read_json(&dir.join(".claude.json")).unwrap();
        assert_eq!(config["oauthAccount"]["accountUuid"], "user-1", "the profile knows whose login it holds");

        // The CLI in the project renews its login: the vault takes the newer one.
        write_login(&claude("r2", 2_000), &profile_source(&dir)).unwrap();
        sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
        assert_eq!(stored(&vault.0, &slot).as_deref(), Some("r2"));
        assert_eq!(in_profile(&root.0, &slot).as_deref(), Some("r2"));

        // A newer login in the vault (renewed while it was the default) replaces an older profile copy.
        save_slot_in(&vault.0, &claude("r3", 3_000)).unwrap();
        sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
        assert_eq!(in_profile(&root.0, &slot).as_deref(), Some("r3"));
    }

    #[test]
    fn an_account_in_use_by_the_cli_is_never_also_signed_in_to_its_profile() {
        let (vault, root) = (TempDir::new(), TempDir::new());
        let slot = claude("r1", 1_000);
        save_slot_in(&vault.0, &slot).unwrap();
        let wanted = [(Provider::Claude, slot.id.clone())];
        sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
        let dir = root.0.join(profile_name(Provider::Claude, &slot.id));
        write_login(&claude("r2", 2_000), &profile_source(&dir)).unwrap();

        let mut active = Active::new();
        active.insert((Provider::Claude, slot.id.clone()), vec!["Windows".into()]);
        let states = sync_profiles_in(&vault.0, &root.0, &wanted, &active);
        assert_eq!(states[&wanted[0]], ProfileState::AtDefault);
        assert!(!profile_login_file(Provider::Claude, &dir).exists(), "one live copy only");
        assert_eq!(stored(&vault.0, &slot).as_deref(), Some("r2"), "the profile's renewal was kept first");
        assert!(dir.join(".claude.json").exists(), "the rest of the profile stays");

        // Switched away again: the profile is signed in from the vault.
        assert_eq!(sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new())[&wanted[0]], ProfileState::Ready);
        assert_eq!(in_profile(&root.0, &slot).as_deref(), Some("r2"));
    }

    #[test]
    fn switching_to_a_pinned_account_takes_its_newer_login_back_from_the_profile() {
        let (vault, root) = (TempDir::new(), TempDir::new());
        let slot = claude("r1", 1_000);
        save_slot_in(&vault.0, &slot).unwrap();
        let dir = root.0.join(profile_name(Provider::Claude, &slot.id));
        write_profile(&claude("r2", 2_000), &dir).unwrap();
        let taken = reclaim_profile(&vault.0, &root.0, &slot).expect("the profile's login is newer");
        assert_eq!(refresh_token(&taken), Some("r2"));
        assert_eq!(stored(&vault.0, &slot).as_deref(), Some("r2"));
        assert!(!profile_login_file(Provider::Claude, &dir).exists());

        // An older copy is dropped and the vault's login is used.
        write_profile(&claude("r0", 500), &dir).unwrap();
        assert!(reclaim_profile(&vault.0, &root.0, &claude("r2", 2_000)).is_none());
        assert!(!profile_login_file(Provider::Claude, &dir).exists());
        // No profile at all is fine.
        assert!(reclaim_profile(&vault.0, &TempDir::new().0, &slot).is_none());
    }

    #[test]
    fn unpinned_profiles_lose_their_login_and_removed_accounts_stay_removed() {
        let (vault, root) = (TempDir::new(), TempDir::new());
        let kept = claude("r1", 1_000);
        save_slot_in(&vault.0, &kept).unwrap();
        let kept_dir = root.0.join(profile_name(Provider::Claude, &kept.id));
        write_profile(&claude("r2", 2_000), &kept_dir).unwrap();
        let gone = codex("c1", "2026-10-01T00:00:00Z");
        let gone_dir = root.0.join(profile_name(Provider::Codex, &gone.id));
        write_profile(&gone, &gone_dir).unwrap();

        sync_profiles_in(&vault.0, &root.0, &[], &Active::new());
        assert!(!profile_login_file(Provider::Claude, &kept_dir).exists() && !profile_login_file(Provider::Codex, &gone_dir).exists());
        assert_eq!(stored(&vault.0, &kept).as_deref(), Some("r2"), "a renewal is not lost on unpinning");
        assert!(stored(&vault.0, &gone).is_none(), "an account missing from the vault is not brought back");
        assert!(gone_dir.join("config.toml").exists());
    }

    #[test]
    fn codex_profiles_use_their_own_auth_file_and_renewal_time() {
        let (vault, root) = (TempDir::new(), TempDir::new());
        let slot = codex("c1", "2026-10-01T00:00:00Z");
        save_slot_in(&vault.0, &slot).unwrap();
        let wanted = [(Provider::Codex, slot.id.clone())];
        assert_eq!(sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new())[&wanted[0]], ProfileState::Ready);
        let dir = root.0.join(profile_name(Provider::Codex, &slot.id));
        assert!(std::fs::read_to_string(dir.join("config.toml")).unwrap().contains("cli_auth_credentials_store = \"file\""));
        assert_eq!(in_profile(&root.0, &slot).as_deref(), Some("c1"));
        // A stale profile copy (older renewal) never overwrites the vault.
        save_slot_in(&vault.0, &codex("c2", "2026-10-05T00:00:00Z")).unwrap();
        write_login(&codex("c0", "2026-09-01T00:00:00Z"), &profile_source(&dir)).unwrap();
        sync_profiles_in(&vault.0, &root.0, &wanted, &Active::new());
        assert_eq!(stored(&vault.0, &slot).as_deref(), Some("c2"));
        assert_eq!(in_profile(&root.0, &slot).as_deref(), Some("c2"));
        // No saved login: nothing to sign in with.
        let none = [(Provider::Codex, "missing".to_string())];
        assert_eq!(sync_profiles_in(&vault.0, &root.0, &none, &Active::new())[&none[0]], ProfileState::NoLogin);
    }

    #[test]
    fn profile_names_are_stable_and_hide_the_account_id() {
        assert_eq!(profile_name(Provider::Claude, "user-1:org-1"), profile_name(Provider::Claude, "user-1:org-1"));
        assert_ne!(profile_name(Provider::Claude, "a"), profile_name(Provider::Codex, "a"));
        assert!(!profile_name(Provider::Codex, "user-2:acct-2").contains("user-2"));
    }

    // -- The encrypted vault ----------------------------------------------------------------

    /// A slot as releases before the encrypted vault stored it.
    fn plain(vault: &Path, slot: &Slot) -> PathBuf {
        let path = slot_path_in(vault, slot.provider, &slot.id).with_extension(PLAIN_EXT);
        std::fs::write(&path, serde_json::to_string_pretty(slot).unwrap()).unwrap();
        path
    }

    fn files(vault: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(vault).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    fn real_migrate(vault: &Path) -> VaultMigration {
        migrate_vault_in(vault, crate::private_store::save_secret, crate::private_store::load_secret)
    }

    #[test]
    fn migration_encrypts_every_plaintext_slot_and_leaves_no_token_on_disk() {
        let vault = TempDir::new();
        let (a, b) = (claude("SYNTH-REFRESH-ALPHA", 1_000), codex("SYNTH-REFRESH-BRAVO", "2026-10-01T00:00:00Z"));
        plain(&vault.0, &a);
        plain(&vault.0, &b);
        assert_eq!(real_migrate(&vault.0), VaultMigration { migrated: 2, failed: 0, unreadable: 0 });
        assert!(files(&vault.0).iter().all(|f| f.ends_with(".slot")), "{:?}", files(&vault.0));
        assert_eq!(stored(&vault.0, &a).as_deref(), Some("SYNTH-REFRESH-ALPHA"));
        assert_eq!(stored(&vault.0, &b).as_deref(), Some("SYNTH-REFRESH-BRAVO"));
        // DPAPI on Windows; on other platforms save_secret is a private (0600) plaintext file.
        #[cfg(windows)]
        for entry in std::fs::read_dir(&vault.0).unwrap().flatten() {
            let bytes = std::fs::read(entry.path()).unwrap();
            for token in ["SYNTH-REFRESH-ALPHA", "SYNTH-REFRESH-BRAVO", "at-SYNTH", "test@example.invalid"] {
                assert!(!bytes.windows(token.len()).any(|w| w == token.as_bytes()), "{token} in {:?}", entry.path());
            }
        }
        // Running again finds nothing to do, and saves keep writing encrypted slots only.
        assert!(real_migrate(&vault.0).is_empty());
        save_slot_in(&vault.0, &claude("SYNTH-REFRESH-CHARLIE", 2_000)).unwrap();
        assert_eq!(stored(&vault.0, &a).as_deref(), Some("SYNTH-REFRESH-CHARLIE"));
        assert_eq!(files(&vault.0).len(), 2);
    }

    #[test]
    fn a_failed_migration_keeps_the_plaintext_slot_and_it_is_still_read() {
        let vault = TempDir::new();
        let (a, b) = (claude("SYNTH-REFRESH-ALPHA", 1_000), codex("SYNTH-REFRESH-BRAVO", "2026-10-01T00:00:00Z"));
        let (pa, pb) = (plain(&vault.0, &a), plain(&vault.0, &b));
        // Encrypting the Claude slot fails outright. Match the file name only: the temporary
        // directory can contain "claude-" too (Claude Code's scratchpad paths do).
        let failing = |path: &Path, bytes: &[u8]| {
            if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("claude-")) { Err(std::io::Error::other("disk full")) } else { crate::private_store::save_secret(path, bytes) }
        };
        let result = migrate_vault_in(&vault.0, failing, crate::private_store::load_secret);
        assert_eq!(result, VaultMigration { migrated: 1, failed: 1, unreadable: 0 });
        assert!(pa.exists() && !pa.with_extension(SLOT_EXT).exists());
        assert!(!pb.exists() && pb.with_extension(SLOT_EXT).exists());
        assert_eq!(load_slots_in(&vault.0).len(), 2);
        assert_eq!(stored(&vault.0, &a).as_deref(), Some("SYNTH-REFRESH-ALPHA"));

        // Writing works but the copy does not read back the same: the copy goes, the plaintext stays.
        let garbled = |path: &Path| crate::private_store::load_secret(path).map(|mut b| { b.reverse(); b });
        let result = migrate_vault_in(&vault.0, crate::private_store::save_secret, garbled);
        assert_eq!(result, VaultMigration { migrated: 0, failed: 1, unreadable: 0 });
        assert!(pa.exists() && !pa.with_extension(SLOT_EXT).exists());
        assert_eq!(stored(&vault.0, &a).as_deref(), Some("SYNTH-REFRESH-ALPHA"));

        // The next start succeeds.
        assert_eq!(real_migrate(&vault.0), VaultMigration { migrated: 1, failed: 0, unreadable: 0 });
        assert_eq!(load_slots_in(&vault.0).len(), 2);
    }

    #[test]
    fn unreadable_files_are_left_alone_and_never_stop_the_others() {
        let vault = TempDir::new();
        let a = claude("SYNTH-REFRESH-ALPHA", 1_000);
        plain(&vault.0, &a);
        std::fs::write(vault.0.join("claude-broken.json"), "{ not json").unwrap();
        std::fs::write(vault.0.join("codex-other.json"), r#"{"something":"else"}"#).unwrap();
        std::fs::write(vault.0.join("codex-garbage.slot"), b"\x00\x01not dpapi").unwrap();
        assert_eq!(real_migrate(&vault.0), VaultMigration { migrated: 1, failed: 0, unreadable: 2 });
        assert_eq!(std::fs::read_to_string(vault.0.join("claude-broken.json")).unwrap(), "{ not json");
        assert!(vault.0.join("codex-garbage.slot").exists());
        let slots = load_slots_in(&vault.0);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].id, a.id);
    }

    #[test]
    fn a_newer_plaintext_slot_wins_until_the_next_save_replaces_it() {
        let vault = TempDir::new();
        save_slot_in(&vault.0, &claude("SYNTH-REFRESH-OLD", 1_000)).unwrap();
        let encrypted = slot_path_in(&vault.0, Provider::Claude, &claude("x", 0).id);
        // An earlier release, run after this one, renewed the login and wrote it as plaintext.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&encrypted).unwrap().set_modified(old).unwrap();
        let newer = plain(&vault.0, &claude("SYNTH-REFRESH-NEW", 2_000));
        assert_eq!(load_slots_in(&vault.0).len(), 1);
        assert_eq!(stored(&vault.0, &claude("x", 0)).as_deref(), Some("SYNTH-REFRESH-NEW"));
        // Migration takes the newer plaintext login, not the older encrypted one.
        assert_eq!(real_migrate(&vault.0).migrated, 1);
        assert!(!newer.exists());
        assert_eq!(stored(&vault.0, &claude("x", 0)).as_deref(), Some("SYNTH-REFRESH-NEW"));
        // A save also removes a leftover plaintext copy.
        plain(&vault.0, &claude("SYNTH-REFRESH-STALE", 500));
        save_slot_in(&vault.0, &claude("SYNTH-REFRESH-LATEST", 3_000)).unwrap();
        assert_eq!(files(&vault.0).len(), 1);
        assert_eq!(stored(&vault.0, &claude("x", 0)).as_deref(), Some("SYNTH-REFRESH-LATEST"));
    }
}
