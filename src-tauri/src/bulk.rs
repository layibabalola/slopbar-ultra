//! Bulk account actions: which accounts "Connect monitoring for all" and "Remove lapsed accounts"
//! act on. The app carries them out with the same one-at-a-time operations as each row's buttons.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::accounts::Provider;
use crate::model::{AccountUsage, Snapshot};
use crate::settings::Settings;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub provider: Provider,
    pub id: String,
    /// Alias, else email: what the user signs in as, or sees removed.
    pub account: Option<String>,
    /// Lapsed accounts: the day the plan ended (the last paid day the user entered, or the end
    /// the provider gave).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<NaiveDate>,
}

fn target(a: &AccountUsage, ended: Option<NaiveDate>) -> Target {
    Target { provider: a.provider, id: a.id.clone(), account: a.alias.clone().or(a.email.clone()), ended }
}

/// Accounts without independent monitoring, in the order Settings lists them.
pub fn unmonitored(snapshot: &Snapshot) -> Vec<Target> {
    snapshot.accounts.iter().filter(|a| !a.monitor_connected).map(|a| target(a, None)).collect()
}

/// Whether an account needs a monitoring sign-in: it has none, or its saved one ended, was refused
/// or ends within `monitoring::RENEW_DAYS` (`AccountUsage::monitor_renewal`). Never one known only
/// by a token hash (`token:` ids): there is no account to check a sign-in against, so it would only
/// fail and stop the queue.
pub fn needs_sign_in(a: &AccountUsage) -> bool {
    (!a.monitor_connected || a.monitor_renewal.is_some()) && !a.id.starts_with("token:")
}

/// What "Connect monitoring for all" signs in, in the order Settings lists them: every account
/// that needs a monitoring sign-in (see `needs_sign_in`).
pub fn to_connect(snapshot: &Snapshot) -> Vec<Target> {
    snapshot.accounts.iter().filter(|a| needs_sign_in(a)).map(|a| target(a, None)).collect()
}

/// Whether an account's saved CLI sign-in needs a new one that "Sign in all accounts" makes: it
/// ended or ends within `monitoring::RENEW_DAYS` (`AccountUsage::cli_renewal`), or the provider
/// rejected a Claude one (`rejected_claude_login`), and the account is not in use, whose sign-in the
/// CLI manages. Never one known only by a token hash (`token:` ids): there is no account to check a
/// new sign-in against, so it would only fail and stop the queue.
pub fn needs_cli_sign_in(a: &AccountUsage) -> bool {
    (a.cli_renewal.is_some() || rejected_claude_login(a)) && !a.active && !a.id.starts_with("token:")
}

/// The provider rejected the account's saved Claude login (`cli_rejected`): a new sign-in fixes
/// that. Only Claude's: SlopBar knows when a saved Claude access token expires and never reads usage
/// with an expired one, so a rejection is one; a saved Codex token's expiry is unknown, so a
/// rejection may only mean it aged out, which the CLI renews by itself.
pub fn rejected_claude_login(a: &AccountUsage) -> bool {
    a.cli_rejected && a.provider == Provider::Claude
}

/// The day an account's plan ended, if it has: the user's last paid day is past, or the provider
/// said the plan would not renew and its end has passed.
pub fn ended(a: &AccountUsage, settings: &Settings, now: DateTime<Utc>) -> Option<NaiveDate> {
    let entered = settings.subscriptions.get(&format!("{}:{}", a.provider.key(), a.id)).and_then(|p| p.cancel_after)
        .filter(|d| *d < now.date_naive());
    let b = &a.billing;
    let ending = b.cancels_at.is_some() || b.will_renew == Some(false);
    let provider = ending.then(|| b.cancels_at.or(b.active_until).or(b.renews_at)).flatten()
        .filter(|t| *t <= now).map(|t| t.date_naive());
    entered.or(provider)
}

/// An account a restored backup knows that is not signed in here yet. The backup names it, and
/// that name exists only in the backup file, so it is kept here (`pending-signins.json`, the
/// app's data folder) until the account is signed in again. Never any sign-in data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub provider: Provider,
    pub id: String,
    pub email: Option<String>,
}

fn pending_path() -> Option<std::path::PathBuf> {
    Some(crate::settings::app_dir()?.join("pending-signins.json"))
}

pub fn load_pending() -> Vec<Pending> {
    pending_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_pending(list: &[Pending]) -> std::io::Result<()> {
    let path = pending_path().ok_or_else(|| std::io::Error::other("no data folder"))?;
    if list.is_empty() {
        return match std::fs::remove_file(&path) { Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e), _ => Ok(()) };
    }
    crate::private_store::write(&path, &serde_json::to_vec_pretty(list)?)
}

/// Adds a restore's accounts that are not signed in here to `list`, once each.
pub fn add_pending(list: &mut Vec<Pending>, restored: impl IntoIterator<Item = Pending>) {
    for p in restored {
        match list.iter_mut().find(|q| q.provider == p.provider && q.id == p.id) {
            Some(q) => { if p.email.is_some() { q.email = p.email; } }
            None => list.push(p),
        }
    }
}

/// One sign-in of "Sign in all accounts": `Add` saves a pending account's login in the vault (the
/// account appears), `Renew` saves a new CLI login for an account whose saved one ended or ends soon
/// (`needs_cli_sign_in`) the same way, and `Connect` gives an account its monitoring sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Add,
    Renew,
    Connect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub kind: Kind,
    #[serde(flatten)]
    pub target: Target,
    /// Who must sign in: the account's email, prefilled where the CLI can.
    pub email: Option<String>,
    /// Another of the user's computers whose sign-in this is, completed in this computer's browser
    /// ("Sign in all your computers from here", see `relay`); None: this computer's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<Machine>,
}

/// Another of the user's computers, as its own SlopBar names it, and the SSH host that reaches it
/// from here (kept here, never shown).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub id: String,
    pub name: String,
    #[serde(skip)]
    pub host: String,
}

/// The sign-ins for one email, run back to back in that email's browser profile, so Google is
/// asked once for all of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub email: Option<String>,
    pub steps: Vec<Step>,
}

/// What "Sign in all accounts" does: every pending account signed in again, a new CLI sign-in for
/// each account not in use whose saved one ended or ends soon (`needs_cli_sign_in`), and, with
/// `monitoring`, a monitoring sign-in for each account that needs one (`needs_sign_in`: none yet,
/// or one ended, refused or ending soon), grouped by email in the order the emails first appear.
/// Within an email: Claude before Codex, each account's own sign-in before its monitoring one. An
/// account without an email is a group of its own.
pub fn sign_in_plan(snapshot: &Snapshot, pending: &[Pending], monitoring: bool) -> Vec<Group> {
    let mut steps: Vec<Step> = Vec::new();
    for p in pending {
        if snapshot.accounts.iter().any(|a| a.provider == p.provider && a.id == p.id) {
            continue;
        }
        let target = Target { provider: p.provider, id: p.id.clone(), account: p.email.clone(), ended: None };
        steps.push(Step { kind: Kind::Add, target: target.clone(), email: p.email.clone(), machine: None });
        if monitoring {
            steps.push(Step { kind: Kind::Connect, target, email: p.email.clone(), machine: None });
        }
    }
    for a in &snapshot.accounts {
        if needs_cli_sign_in(a) {
            steps.push(Step { kind: Kind::Renew, target: target(a, None), email: a.email.clone(), machine: None });
        }
        if monitoring && needs_sign_in(a) {
            steps.push(Step { kind: Kind::Connect, target: target(a, None), email: a.email.clone(), machine: None });
        }
    }
    by_email(steps)
}

/// What "Connect monitoring for all" does: a monitoring sign-in for each account that needs one
/// (`needs_sign_in`), grouped as `sign_in_plan` groups them. Never a CLI sign-in: that is "Sign in
/// all accounts".
pub fn connect_plan(snapshot: &Snapshot) -> Vec<Group> {
    by_email(snapshot.accounts.iter().filter(|a| needs_sign_in(a))
        .map(|a| Step { kind: Kind::Connect, target: target(a, None), email: a.email.clone(), machine: None }).collect())
}

/// Which email group a step joins: its email, whatever its case; an account without one is a
/// group of its own (on its own computer).
fn email_key(s: &Step) -> String {
    s.email.as_deref().map(|e| e.trim().to_lowercase()).filter(|e| !e.is_empty())
        .unwrap_or_else(|| format!("\0{}:{}:{}", s.machine.as_ref().map_or("", |m| m.id.as_str()), s.target.provider.key(), s.target.id))
}

/// `steps` in groups by email, in the order the emails first appear.
fn grouped(steps: Vec<Step>) -> Vec<Group> {
    let mut groups: Vec<(String, Group)> = Vec::new();
    for step in steps {
        let k = email_key(&step);
        match groups.iter_mut().find(|(g, _)| *g == k) {
            Some((_, g)) => g.steps.push(step),
            None => groups.push((k, Group { email: step.email.clone(), steps: vec![step] })),
        }
    }
    groups.into_iter().map(|(_, g)| g).collect()
}

/// Steps grouped by email in the order the emails first appear, Claude before Codex within one.
fn by_email(steps: Vec<Step>) -> Vec<Group> {
    let mut groups = grouped(steps);
    for g in &mut groups {
        // Stable: an account's Add stays before its Connect, accounts keep their order.
        g.steps.sort_by_key(|s| s.target.provider != Provider::Claude);
    }
    groups
}

/// "Sign in all your computers from here" (see `relay`): this computer's plan and each other
/// computer's, merged by email in the order the emails first appear (this computer's first), so an
/// email's sign-ins on every computer run back to back in its one browser profile here and Google
/// is asked once per email for all of them. Within an email this computer's steps come first, then
/// each other computer's in `others`' order, each in its own plan's order (Claude before Codex, an
/// account's own sign-in before its monitoring one).
pub fn everywhere(here: Vec<Group>, others: Vec<(Machine, Vec<Group>)>) -> Vec<Group> {
    let mut steps: Vec<Step> = here.into_iter().flat_map(|g| g.steps).map(|s| Step { machine: None, ..s }).collect();
    for (machine, plan) in others {
        steps.extend(plan.into_iter().flat_map(|g| g.steps).map(|s| Step { machine: Some(machine.clone()), ..s }));
    }
    // Stable within each email: computers keep this order, and each its own steps' order.
    grouped(steps)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Lapsed {
    /// Can be removed now.
    pub remove: Vec<Target>,
    /// The CLI is signed in to them: switch away first.
    pub in_use: Vec<Target>,
}

pub fn lapsed(snapshot: &Snapshot, settings: &Settings, now: DateTime<Utc>) -> Lapsed {
    let mut out = Lapsed::default();
    for a in &snapshot.accounts {
        if let Some(day) = ended(a, settings, now) {
            if a.active { out.in_use.push(target(a, Some(day))) } else { out.remove.push(target(a, Some(day))) }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Subscription;

    fn account(id: &str, active: bool, monitored: bool) -> AccountUsage {
        serde_json::from_value(serde_json::json!({
            "id": id, "provider": "claude", "email": format!("{id}@example.invalid"), "alias": null, "plan": "max", "org": null,
            "active": active, "sources": [], "windows": [], "credits": null, "resets": [], "error": null,
            "stale": false, "fetched_at": null, "monitor_connected": monitored,
        })).unwrap()
    }

    #[test]
    fn unmonitored_lists_only_accounts_without_monitoring() {
        let mut named = account("b", false, false);
        named.alias = Some("Spare".into());
        let snapshot = Snapshot { accounts: vec![account("a", true, true), named, account("c", true, false)], updated_at: None };
        let found = unmonitored(&snapshot);
        assert_eq!(found.iter().map(|t| (t.id.as_str(), t.account.as_deref())).collect::<Vec<_>>(),
            [("b", Some("Spare")), ("c", Some("c@example.invalid"))]);
    }

    #[test]
    fn lapsed_means_past_the_last_paid_day_or_a_provider_end_that_passed() {
        let now: DateTime<Utc> = "2026-10-21T12:00:00Z".parse().unwrap();
        let mut settings = Settings::default();
        let day = |s: &str| s.parse::<NaiveDate>().unwrap();
        settings.subscriptions.insert("claude:entered".into(), Subscription { cancel_after: Some(day("2026-10-20")), ..Default::default() });
        settings.subscriptions.insert("claude:today".into(), Subscription { cancel_after: Some(day("2026-10-21")), ..Default::default() });
        settings.subscriptions.insert("claude:busy".into(), Subscription { cancel_after: Some(day("2026-10-01")), ..Default::default() });
        let mut cancelled = account("cancelled", false, true);
        cancelled.billing.cancels_at = Some("2026-10-18T00:00:00Z".parse().unwrap());
        let mut ending = account("ending", false, true);
        ending.billing.will_renew = Some(false);
        ending.billing.active_until = Some("2026-10-30T00:00:00Z".parse().unwrap());
        let mut renewing = account("renewing", false, true);
        renewing.billing.active_until = Some("2026-10-01T00:00:00Z".parse().unwrap());
        let snapshot = Snapshot { accounts: vec![
            account("entered", false, false), account("today", false, false), account("busy", true, true), cancelled, ending, renewing,
        ], updated_at: None };
        let found = lapsed(&snapshot, &settings, now);
        assert_eq!(found.remove.iter().map(|t| (t.id.as_str(), t.ended)).collect::<Vec<_>>(),
            [("entered", Some(day("2026-10-20"))), ("cancelled", Some(day("2026-10-18")))],
            "the last paid day itself still counts, an end still ahead and a period end without cancelling do not");
        assert_eq!(found.in_use.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["busy"], "the account in use is never removed");
    }

    fn pending(provider: Provider, id: &str, email: Option<&str>) -> Pending {
        Pending { provider, id: id.into(), email: email.map(Into::into) }
    }

    fn summary(groups: &[Group]) -> Vec<(Option<&str>, Vec<String>)> {
        groups.iter().map(|g| (g.email.as_deref(), g.steps.iter().map(|s| format!("{:?} {} {}",
            s.kind, s.target.provider.key(), s.target.id)).collect())).collect()
    }

    #[test]
    fn a_restore_signs_in_again_one_email_at_a_time_claude_first() {
        // A new machine: four restored accounts across two emails, Codex listed first.
        let list = [
            pending(Provider::Codex, "cx-one", Some("one@example.invalid")),
            pending(Provider::Claude, "cl-two", Some("two@example.invalid")),
            pending(Provider::Claude, "cl-one", Some("One@Example.invalid")),
            pending(Provider::Codex, "cx-two", Some("two@example.invalid")),
        ];
        let empty = Snapshot { accounts: vec![], updated_at: None };
        let plan = sign_in_plan(&empty, &list, false);
        assert_eq!(summary(&plan), [
            (Some("one@example.invalid"), vec!["Add claude cl-one".to_string(), "Add codex cx-one".into()]),
            (Some("two@example.invalid"), vec!["Add claude cl-two".to_string(), "Add codex cx-two".into()]),
        ], "two emails, two Google sign-ins; the email's case does not split a group");
        let with_monitoring = sign_in_plan(&empty, &list, true);
        assert_eq!(summary(&with_monitoring)[0].1, ["Add claude cl-one", "Connect claude cl-one", "Add codex cx-one", "Connect codex cx-one"]);
        assert_eq!(with_monitoring[0].steps[0].email.as_deref(), Some("One@Example.invalid"), "each step keeps the account's own email to prefill");
    }

    #[test]
    fn signed_in_accounts_are_not_pending_and_only_unmonitored_ones_connect() {
        let mut here = account("cl-one", false, false);
        here.email = Some("one@example.invalid".into());
        let mut monitored = account("cl-three", false, true);
        monitored.email = Some("one@example.invalid".into());
        let mut nameless = account("cl-four", false, false);
        nameless.email = None;
        let snapshot = Snapshot { accounts: vec![here, monitored, nameless], updated_at: None };
        let list = [pending(Provider::Claude, "cl-one", Some("one@example.invalid")), pending(Provider::Codex, "cx-one", Some("one@example.invalid"))];
        assert_eq!(summary(&sign_in_plan(&snapshot, &list, true)), [
            (Some("one@example.invalid"), vec!["Connect claude cl-one".to_string(), "Add codex cx-one".into(), "Connect codex cx-one".into()]),
            (None, vec!["Connect claude cl-four".to_string()]),
        ]);
        assert!(sign_in_plan(&snapshot, &list[..1], false).is_empty(), "nothing pending, nothing to sign in");
    }

    #[test]
    fn monitoring_that_ended_or_ends_soon_is_signed_in_again_with_the_unmonitored() {
        use crate::model::RenewalKind;
        let mut ended = account("ended", false, true);
        ended.monitor_renewal = Some(RenewalKind::Ended);
        let mut refused = account("refused", false, true);
        refused.provider = Provider::Codex;
        refused.monitor_renewal = Some(RenewalKind::Refused);
        let mut token = account("token:abc", false, false);
        token.email = None;
        let snapshot = Snapshot { accounts: vec![account("none", false, false), ended, refused, account("fine", true, true), token], updated_at: None };
        let ids = |list: Vec<Target>| list.into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(to_connect(&snapshot)), ["none", "ended", "refused"], "never a token-only account: it cannot connect");
        assert_eq!(ids(unmonitored(&snapshot)), ["none", "token:abc"], "unmonitored still means without monitoring");
        assert_eq!(summary(&sign_in_plan(&snapshot, &[], true)), [
            (Some("none@example.invalid"), vec!["Connect claude none".to_string()]),
            (Some("ended@example.invalid"), vec!["Connect claude ended".to_string()]),
            (Some("refused@example.invalid"), vec!["Connect codex refused".to_string()]),
        ]);
        assert!(sign_in_plan(&snapshot, &[], false).is_empty(), "without monitoring, nothing to sign in");
    }

    #[test]
    fn a_saved_cli_sign_in_that_ended_or_ends_soon_is_signed_in_again_before_its_monitoring() {
        use crate::model::RenewalKind;
        let mut ended = account("ended", false, true);
        ended.cli_renewal = Some(RenewalKind::Ended);
        ended.monitor_renewal = Some(RenewalKind::Ended);
        // The same email's Codex account, listed first: Claude's sign-ins still come first.
        let mut codex = account("cx", false, false);
        codex.provider = Provider::Codex;
        codex.email = ended.email.clone();
        let mut ending = account("ending", false, true);
        ending.cli_renewal = Some(RenewalKind::Ending);
        // Never set for the account in use (`accounts::cli_renewal`); were it set, still never touched.
        let mut in_use = account("in-use", true, true);
        in_use.cli_renewal = Some(RenewalKind::Ended);
        let mut token = account("token:abc", false, true);
        token.cli_renewal = Some(RenewalKind::Ended);
        let snapshot = Snapshot { accounts: vec![codex, ended, ending, in_use, token, account("fine", false, true)], updated_at: None };
        assert_eq!(summary(&sign_in_plan(&snapshot, &[], true)), [
            (Some("ended@example.invalid"), vec!["Renew claude ended".to_string(), "Connect claude ended".into(), "Connect codex cx".into()]),
            (Some("ending@example.invalid"), vec!["Renew claude ending".to_string()]),
        ], "the account in use and a token-only account are never signed in again");
        assert_eq!(summary(&sign_in_plan(&snapshot, &[], false)), [
            (Some("ended@example.invalid"), vec!["Renew claude ended".to_string()]),
            (Some("ending@example.invalid"), vec!["Renew claude ending".to_string()]),
        ], "a CLI sign-in is not monitoring: it is made without the monitoring box too");
        assert_eq!(summary(&connect_plan(&snapshot)), [
            (Some("ended@example.invalid"), vec!["Connect claude ended".to_string(), "Connect codex cx".into()]),
        ], "Connect monitoring for all never makes a CLI sign-in");
        assert_eq!(serde_json::to_value(Kind::Renew).unwrap(), "renew");
        assert!(needs_cli_sign_in(&snapshot.accounts[1]) && !needs_cli_sign_in(&snapshot.accounts[3]) && !needs_cli_sign_in(&snapshot.accounts[4]));
        // A saved Claude login the provider rejected is signed in again the same way, never the one
        // in use; a Codex one may only have aged out, which its CLI renews by itself.
        let mut rejected = account("rejected", false, true);
        rejected.cli_rejected = true;
        assert!(needs_cli_sign_in(&rejected));
        rejected.provider = Provider::Codex;
        assert!(!needs_cli_sign_in(&rejected));
        rejected.provider = Provider::Claude;
        rejected.active = true;
        assert!(!needs_cli_sign_in(&rejected));
    }

    #[test]
    fn every_computers_sign_ins_run_by_email_this_computers_first() {
        let step = |kind: Kind, provider: Provider, id: &str, email: Option<&str>| Step {
            kind, target: Target { provider, id: id.into(), account: email.map(Into::into), ended: None }, email: email.map(Into::into), machine: None,
        };
        let machine = |name: &str| Machine { id: format!("id-{name}"), name: name.into(), host: name.to_lowercase() };
        let here = vec![Group { email: Some("b@example.invalid".into()), steps: vec![step(Kind::Renew, Provider::Claude, "b", Some("b@example.invalid"))] }];
        let desk = vec![
            Group { email: Some("A@example.invalid".into()), steps: vec![
                step(Kind::Renew, Provider::Claude, "a", Some("A@example.invalid")),
                step(Kind::Connect, Provider::Claude, "a", Some("A@example.invalid")),
                step(Kind::Connect, Provider::Codex, "ax", Some("A@example.invalid")),
            ] },
            Group { email: Some("b@example.invalid".into()), steps: vec![step(Kind::Connect, Provider::Claude, "b", Some("b@example.invalid"))] },
            Group { email: None, steps: vec![step(Kind::Connect, Provider::Claude, "n", None)] },
        ];
        let mini = vec![
            Group { email: Some("a@example.invalid".into()), steps: vec![step(Kind::Renew, Provider::Claude, "a", Some("a@example.invalid"))] },
            Group { email: None, steps: vec![step(Kind::Connect, Provider::Claude, "n", None)] },
        ];
        let plan = everywhere(here, vec![(machine("DESK"), desk), (machine("MINI"), mini)]);
        let seen: Vec<(Option<&str>, Vec<String>)> = plan.iter().map(|g| (g.email.as_deref(), g.steps.iter().map(|s| format!("{} {:?} {}",
            s.machine.as_ref().map_or("here", |m| m.name.as_str()), s.kind, s.target.id)).collect())).collect();
        assert_eq!(seen, [
            (Some("b@example.invalid"), vec!["here Renew b".to_string(), "DESK Connect b".into()]),
            (Some("A@example.invalid"), vec!["DESK Renew a".to_string(), "DESK Connect a".into(), "DESK Connect ax".into(), "MINI Renew a".into()]),
            (None, vec!["DESK Connect n".to_string()]),
            (None, vec!["MINI Connect n".to_string()]),
        ], "one group per email across computers (its case aside), this computer's first, each computer's steps in its own order; \
            an account without an email is a group of its own on each computer");
        // What another computer's preview sends reads back as a plan.
        let sent = serde_json::json!([{ "email": "a@example.invalid", "steps": [{ "kind": "renew", "provider": "claude", "id": "a", "account": "a@example.invalid", "email": "a@example.invalid" }] }]);
        let read: Vec<Group> = serde_json::from_value(sent).unwrap();
        assert_eq!((read[0].steps[0].kind, read[0].steps[0].machine.clone()), (Kind::Renew, None));
        // A relayed step names its computer, never the host that reaches it.
        let shown = serde_json::to_value(&plan[0].steps[1]).unwrap();
        assert_eq!(shown["machine"], serde_json::json!({ "id": "id-DESK", "name": "DESK" }));
    }

    #[test]
    fn restores_add_pending_accounts_once() {
        let mut list = vec![pending(Provider::Claude, "a", None)];
        add_pending(&mut list, [pending(Provider::Claude, "a", Some("a@example.invalid")), pending(Provider::Codex, "a", None)]);
        add_pending(&mut list, [pending(Provider::Claude, "a", None)]);
        assert_eq!(list, [pending(Provider::Claude, "a", Some("a@example.invalid")), pending(Provider::Codex, "a", None)],
            "a later restore without the email keeps the one known; the same id of another provider is another account");
    }
}
