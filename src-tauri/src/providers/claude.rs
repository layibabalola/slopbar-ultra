//! Claude usage from a saved login's OAuth token.

use chrono::{DateTime, Utc};
use serde::Deserialize;

use serde_json::Value;

use crate::model::{ExtraUsage, ResetGrant, UsageWindow};

#[derive(Deserialize)]
struct Usage {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
    seven_day_opus: Option<Window>,
    seven_day_sonnet: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    utilization: Option<f64>,
    resets_at: Option<DateTime<Utc>>,
}

pub struct ClaudeUsage {
    pub windows: Vec<UsageWindow>,
    /// The account's banked resets, when this reading asked for its reset status and the reply
    /// carried one; None otherwise, so the last ones read are kept (see `fetch`).
    pub resets: Option<Vec<ResetGrant>>,
    /// That status said Claude lists no resets to this client (`AccountUsage::resets_withheld`).
    pub withheld: bool,
    /// The reading asked for the status and that request failed, so it was taken again without it.
    pub refused: bool,
    pub billing: Option<crate::model::BillingHints>,
    pub extra_usage: Option<ExtraUsage>,
}

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The same reading with the account's reset status (`cedar_ember`) added, as Claude Code's
/// /limit-reset asks for it; nothing else in the reply changes (seen live on 8 accounts). Without
/// the parameter the status is always null.
const RESET_STATUS_URL: &str = "https://api.anthropic.com/api/oauth/usage?cedar_ember=1";

/// The plan facts in the profile: when the subscription started, its status and tier. Claude
/// exposes no renewal date or cancellation to this token.
fn billing(raw: &Value) -> crate::model::BillingHints {
    let org = &raw["organization"];
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(String::from);
    crate::model::BillingHints {
        tier: text(&org["rate_limit_tier"]),
        billing_type: text(&org["billing_type"]),
        started_at: org["subscription_created_at"].as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|t| t.with_timezone(&Utc)),
        status: text(&org["subscription_status"]),
        fetched_at: Some(Utc::now()),
        ..Default::default()
    }
}

/// Fetches the usage windows, or an error code (see `model::AccountUsage::error`). With
/// `ask_resets` (`providers::ask_resets`) the reading also asks for the account's reset status, in the
/// same request; if that request fails for any reason but a 429 (a rejected token included: only a
/// plain reading may call a token invalid), the reading is taken again without it, so the
/// parameter never costs the usage numbers. SlopBar never asks at a limit wall (`at_wall`) and
/// never claims a reset: the user does, in Claude Code or on claude.ai.
pub async fn fetch(client: &reqwest::Client, token: &str, slot: &crate::accounts::Slot, ask_resets: bool, observe: &super::evidence::Observer<'_>) -> Result<ClaudeUsage, String> {
    // Read-only identity evidence and plan facts every twelve hours per account, using the same
    // access token the monitor supplied. Never refresh, replace or persist a login here.
    let mut plan = None;
    if super::billing_due(&format!("claude:{}", slot.id)) {
        let response = client.get("https://api.anthropic.com/api/oauth/profile")
            .bearer_auth(token).header("anthropic-beta", "oauth-2025-04-20").send().await;
        match response {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 429 {
                    observe(super::evidence::Evidence::outcome(super::evidence::Endpoint::ClaudeProfile, Some(status)));
                    return Err(super::rate_limited(&resp));
                }
                if resp.status().is_success() {
                    match resp.json::<Value>().await {
                        Ok(raw) => { plan = Some(billing(&raw)); observe(super::evidence::profile(status, &raw, slot)) }
                        Err(_) => observe(super::evidence::Evidence::outcome(super::evidence::Endpoint::ClaudeProfile, Some(status))),
                    }
                } else { observe(super::evidence::Evidence::outcome(super::evidence::Endpoint::ClaudeProfile, Some(status))); }
            }
            Err(_) => observe(super::evidence::Evidence::outcome(super::evidence::Endpoint::ClaudeProfile, None)),
        }
    }
    let read = |url: &'static str| client.get(url).bearer_auth(token).header("anthropic-beta", "oauth-2025-04-20").send();
    let mut resp = read(if ask_resets { RESET_STATUS_URL } else { USAGE_URL }).await;
    let failed = |r: &reqwest::Result<reqwest::Response>| r.as_ref().map_or(true, |r| !r.status().is_success() && r.status().as_u16() != 429);
    let refused = ask_resets && failed(&resp);
    if refused {
        observe(super::evidence::Evidence::outcome(super::evidence::Endpoint::ClaudeUsage, resp.as_ref().ok().map(|r| r.status().as_u16())));
        resp = read(USAGE_URL).await;
    }
    let resp = resp.map_err(|e| crate::net::failure_text(&e))?;
    if resp.status() == 401 {
        return Err("token_invalid".into());
    }
    if resp.status() == 429 {
        return Err(super::rate_limited(&resp));
    }
    if !resp.status().is_success() {
        return Err(format!("http_{}", resp.status().as_u16()));
    }
    let status = resp.status().as_u16();
    let raw: Value = match resp.json().await {
        Ok(raw) => raw,
        Err(_) => { observe(super::evidence::Evidence::outcome(super::evidence::Endpoint::ClaudeUsage, Some(status))); return Err("bad_response".into()); }
    };
    observe(super::evidence::capture(super::evidence::Endpoint::ClaudeUsage, status, &raw));
    // Only a reply that carries the status says which resets the account holds: one without it (a
    // reading that did not ask, or a server that ignored the parameter) keeps the last ones read.
    let resets = raw["cedar_ember"].is_object().then(|| resets(&raw));
    let withheld = withheld(&raw);
    let extra_usage = extra_usage(&raw);
    let u: Usage = serde_json::from_value(raw).map_err(|_| "bad_response".to_string())?;
    let windows = [
        ("session", u.five_hour),
        ("weekly", u.seven_day),
        ("weekly_opus", u.seven_day_opus),
        ("weekly_sonnet", u.seven_day_sonnet),
    ];
    let windows = windows
        .into_iter()
        .filter_map(|(kind, w)| {
            let w = w?;
            Some(UsageWindow {
                kind: kind.into(),
                window_seconds: None,
                used_percent: w.utilization?,
                resets_at: w.resets_at,
            })
        })
        .collect();
    Ok(ClaudeUsage { windows, resets, withheld, refused, billing: plan, extra_usage })
}

/// Extra usage (`extra_usage`: is_enabled, monthly_limit, used_credits, utilization), read
/// leniently: any field may be missing, null or a numeric string, and the whole object may be
/// absent (None). Amounts come in cents and are returned in dollars; a missing utilization is
/// worked out from the two amounts.
pub(crate) fn extra_usage(raw: &Value) -> Option<ExtraUsage> {
    let e = raw.get("extra_usage").filter(|v| v.is_object())?;
    let number = |snake: &str, camel: &str| -> Option<f64> {
        let v = field(e, snake, camel);
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).filter(|n: &f64| n.is_finite() && *n >= 0.0)
    };
    let monthly_limit = number("monthly_limit", "monthlyLimit").map(|c| c / 100.0);
    let used = number("used_credits", "usedCredits").map(|c| c / 100.0);
    let utilization = number("utilization", "utilization")
        .or_else(|| used.zip(monthly_limit).filter(|(_, l)| *l > 0.0).map(|(u, l)| u / l * 100.0));
    let enabled = field(e, "is_enabled", "isEnabled").as_bool();
    if enabled.is_none() && monthly_limit.is_none() && used.is_none() && utilization.is_none() { return None; }
    Some(ExtraUsage { enabled, monthly_limit, used, utilization })
}

/// A field under either of its spellings (the API is snake_case; be lenient).
fn field<'a>(v: &'a Value, snake: &str, camel: &str) -> &'a Value {
    match v.get(snake) {
        Some(x) if !x.is_null() => x,
        _ => v.get(camel).unwrap_or(&Value::Null),
    }
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc))
}

/// The limits Claude names in a reset's `clears` (Claude Code ignores any other name).
const LIMITS: [&str; 8] = ["five_hour", "seven_day", "seven_day_overage_included", "seven_day_opus", "seven_day_sonnet",
    "seven_day_cowork", "seven_day_omelette", "seven_day_oauth_apps"];

/// At most this many grants are read, each holding at most `MAX_RESETS` resets: no account holds
/// more, and sums of them never overflow.
const MAX_GRANTS: usize = 16;
const MAX_RESETS: u64 = 999;

/// What a granted reset refills, named as Claude Code names it: a Fable reset
/// (`gives_back_weekly`) returns to the weekly limit what Fable used this week; any other is the
/// grant's `type` ("session", "weekly" or "full"; Claude Code ignores any other value), or else
/// follows the limits it clears: the 5-hour limit only ("session"), weekly limits only ("weekly"),
/// or both ("full"). A grant that clears no limit SlopBar knows is "unknown", where Claude Code
/// says "full": SlopBar never claims a reach it cannot see.
fn grant_kind(g: &Value) -> &'static str {
    if field(g, "gives_back_weekly", "givesBackWeekly").as_bool() == Some(true) { return "fable"; }
    match field(g, "type", "type").as_str() {
        Some("session") => return "session",
        Some("weekly") => return "weekly",
        Some("full") => return "full",
        _ => {}
    }
    let clears: Vec<&str> = field(g, "clears", "clears").as_array()
        .map(|a| a.iter().filter_map(Value::as_str).filter(|l| LIMITS.contains(l)).collect()).unwrap_or_default();
    let session = clears.contains(&"five_hour");
    let weekly = clears.iter().any(|l| *l != "five_hour");
    match (session, weekly) { (true, false) => "session", (false, true) => "weekly", (true, true) => "full", (false, false) => "unknown" }
}

/// Whether the reset status says Claude lists no resets to this client: "surface" (it lists them
/// only to Claude Code and claude.ai) or "cli_version" (only to a newer Claude Code).
fn withheld(raw: &Value) -> bool {
    let cedar = &raw["cedar_ember"];
    cedar["eligible"].as_bool() == Some(false) && matches!(cedar["ineligible_reason"].as_str(), Some("surface" | "cli_version"))
}

/// Claude's usage-limit resets. Granted ones come in the reset status (`cedar_ember`) that the
/// usage endpoint fills in only when asked for it (`?cedar_ember=1`, as Claude Code's
/// /limit-reset reads it): each grant holds resets of one kind (`grant_kind`), with the number
/// left and the date to use them by. `juniper_tide` is a 5-hour reset that comes back
/// periodically. Both are null when the account has none. Read leniently: these fields are
/// undocumented.
fn resets(raw: &Value) -> Vec<ResetGrant> {
    let mut out = Vec::new();
    let cedar = &raw["cedar_ember"];
    if let Some(grants) = cedar.get("grants").and_then(Value::as_array) {
        for g in grants.iter().take(MAX_GRANTS) {
            // As Claude Code holds a grant: one without a whole count of resets left is malformed, a
            // paused one is not held, and one with none left still is while it can be used now.
            let Some(left) = field(g, "resets_left", "resetsLeft").as_u64() else { continue };
            let usable = field(g, "usable_now", "usableNow").as_bool().unwrap_or(false);
            if field(g, "paused", "paused").as_bool() == Some(true) || (left == 0 && !usable) {
                continue;
            }
            out.push(ResetGrant {
                kind: grant_kind(g).into(),
                // Claude's own label is not shown: SlopBar names the kind, in the user's language, and
                // a label could say more than the grant does (a Fable reset called "Weekly reset").
                title: None,
                count: left.clamp(1, MAX_RESETS) as u32,
                usable,
                expires_at: time(field(g, "ends_at", "endsAt")),
                next_available_at: None,
            });
        }
    }
    let juniper = &raw["juniper_tide"];
    if juniper.is_object() {
        let next = time(field(juniper, "next_available_at", "nextAvailableAt"));
        let available = field(juniper, "available", "available").as_bool()
            .or(field(juniper, "eligible", "eligible").as_bool())
            .unwrap_or(next.is_none());
        if available || next.is_some() {
            out.push(ResetGrant {
                kind: "session".into(),
                title: None,
                count: u32::from(available),
                usable: available,
                expires_at: None,
                next_available_at: next,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_usage_reads_dollars_and_tolerates_missing_fields() {
        let fixture: Value = serde_json::from_str(include_str!("fixtures/usage-divergence.json")).unwrap();
        let e = extra_usage(&fixture["claude_usage"]).unwrap();
        assert_eq!((e.enabled, e.monthly_limit, e.used, e.utilization), (Some(false), Some(400.0), Some(73.47), Some(18.3675)));
        assert!(!e.active());
        // Absent, null or not an object: nothing to show.
        for raw in [serde_json::json!({}), serde_json::json!({ "extra_usage": null }), serde_json::json!({ "extra_usage": 5 }), serde_json::json!({ "extra_usage": {} })] {
            assert_eq!(extra_usage(&raw), None, "{raw}");
        }
        // Utilization missing: worked out from the amounts. Strings and camelCase are read too.
        let e = extra_usage(&serde_json::json!({ "extra_usage": { "isEnabled": true, "monthly_limit": "20000", "usedCredits": 15000 } })).unwrap();
        assert_eq!((e.enabled, e.monthly_limit, e.used, e.utilization), (Some(true), Some(200.0), Some(150.0), Some(75.0)));
        // A limit of zero gives no utilization; nonsense numbers are dropped.
        let e = extra_usage(&serde_json::json!({ "extra_usage": { "is_enabled": true, "monthly_limit": 0, "used_credits": -3, "utilization": null } })).unwrap();
        assert_eq!((e.monthly_limit, e.used, e.utilization), (Some(0.0), None, None));
        let e = extra_usage(&serde_json::json!({ "extra_usage": { "is_enabled": true } })).unwrap();
        assert!(e.active() && e.monthly_limit.is_none());
    }

    /// A reset status shaped as Claude answers `?cedar_ember=1` (synthetic): one grant per case.
    fn status(grants: Value) -> Value {
        serde_json::json!({
            "five_hour": { "utilization": 100, "resets_at": "2026-11-23T14:00:00Z" },
            "seven_day": { "utilization": 40, "resets_at": "2026-11-27T06:00:00Z" },
            "cedar_ember": { "eligible": true, "ineligible_reason": null, "at_limit": true, "exhausted": ["five_hour"],
                "grants": grants, "next_grant_id": "grant-a", "weekly_resets_at": "2026-11-27T06:00:00Z", "cooldown_until": null },
            "juniper_tide": null,
        })
    }

    #[test]
    fn grants_are_held_and_named_as_claude_code_holds_and_names_them() {
        let raw = status(serde_json::json!([
            { "id": "a", "label": "Weekly reset", "resets_total": 1, "resets_left": 1, "ends_at": "2026-12-01T00:00:00Z",
              "clears": ["seven_day_overage_included"], "usable_now": false, "gives_back_weekly": true, "type": "weekly" },
            { "id": "b", "resets_left": 2, "ends_at": "2026-12-05T00:00:00Z", "clears": ["five_hour", "seven_day"], "usable_now": true },
            { "id": "c", "resets_left": 1, "clears": ["seven_day", "seven_day_opus"] },
            { "id": "d", "resets_left": 1, "clears": ["five_hour"] },
            { "id": "e", "resets_left": 1, "type": "session", "clears": ["seven_day"] },
            { "id": "f", "resets_left": 1, "type": "monthly", "clears": ["five_hour", "a_limit_from_the_future"] },
            { "id": "g", "resetsLeft": 3, "givesBackWeekly": true, "usableNow": true },
            { "id": "h", "resets_left": 0, "usable_now": true, "type": "full" },
            { "id": "i", "resets_left": 0, "clears": ["five_hour"] },
            { "id": "j", "resets_left": 4, "paused": true, "type": "full" },
            { "id": "k", "clears": ["five_hour"] },
            { "id": "l", "resets_left": -1, "type": "full" },
            { "id": "m", "resets_left": 1.5, "type": "full" },
            { "id": "n", "resets_left": 99_999_999_999u64, "clears": ["a_limit_from_the_future"] },
            { "id": "o", "resets_left": 1, "clears": [] },
        ]));
        let got: Vec<(String, u32, bool)> = resets(&raw).into_iter().map(|r| {
            assert_eq!(r.title, None, "Claude's own label is never kept");
            (r.kind, r.count, r.usable)
        }).collect();
        let want: Vec<(String, u32, bool)> = [
            // gives_back_weekly wins over type; type over clears; an unknown type falls to clears.
            ("fable", 1, false), ("full", 2, true), ("weekly", 1, false), ("session", 1, false), ("session", 1, false), ("session", 1, false),
            // camelCase; none left but usable now is held, as one; huge counts are clamped; no known reach is "unknown".
            ("fable", 3, true), ("full", 1, true), ("unknown", 999, false), ("unknown", 1, false),
        ].into_iter().map(|(k, n, u)| (k.to_string(), n, u)).collect();
        assert_eq!(got, want, "paused, spent, missing, negative and fractional counts are not held");
        assert!(!withheld(&raw));
        // At most 16 grants are read.
        let many = status(Value::Array((0..40).map(|i| serde_json::json!({ "id": format!("g{i}"), "resets_left": 999, "type": "full" })).collect()));
        let held = resets(&many);
        assert_eq!((held.len(), held.iter().map(|r| u64::from(r.count)).sum::<u64>()), (16, 16 * 999));
    }

    #[test]
    fn a_status_for_another_client_withholds_the_resets() {
        let surface = serde_json::json!({ "cedar_ember": { "eligible": false, "ineligible_reason": "surface", "at_limit": false, "exhausted": [], "grants": [] } });
        assert!(withheld(&surface) && resets(&surface).is_empty());
        let version = serde_json::json!({ "cedar_ember": { "eligible": false, "ineligible_reason": "cli_version", "grants": [] } });
        assert!(withheld(&version));
        // Not offered to this account (its plan, its seat, no grant): nothing is withheld, there is none.
        for reason in ["tier", "seat", "no_grant", "config_off", "unknown_reason"] {
            assert!(!withheld(&serde_json::json!({ "cedar_ember": { "eligible": false, "ineligible_reason": reason, "grants": [] } })), "{reason}");
        }
        assert!(!withheld(&serde_json::json!({ "cedar_ember": null })));
    }

    /// Live check of this computer's monitored Claude accounts: reads each one's usage plainly and
    /// with `?cedar_ember=1`, using a monitoring access token that stays good for five more minutes
    /// (`monitoring::live_access`: nothing is renewed or saved), and prints the status, SlopBar's
    /// allowlisted evidence of the reset fields (no text, ids or tokens) and the resets it reads.
    /// `SLOPBAR_CLAUDE_LIVE=1 cargo test --lib providers::claude::tests::live_resets -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_resets() {
        use super::super::evidence::{capture, Endpoint};
        if std::env::var_os("SLOPBAR_CLAUDE_LIVE").is_none() { return; }
        let dir = dirs::config_dir().expect("config dir").join("dev.thelio.slopbar").join("monitoring");
        let accounts = crate::monitoring::live_access(&dir, crate::accounts::Provider::Claude);
        println!("{} Claude monitoring sign-ins with a usable access token", accounts.len());
        // As the app's own client (`app::http_client`, desktop builds only): no redirects.
        let client = crate::net::builder().timeout(std::time::Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none()).build().expect("http client");
        for (n, token) in accounts.iter().enumerate() {
            let mut rest = vec![];
            for query in ["", "?cedar_ember=1"] {
                let url = format!("https://api.anthropic.com/api/oauth/usage{query}");
                let resp = client.get(&url).bearer_auth(token.bearer()).header("anthropic-beta", "oauth-2025-04-20").send().await;
                let Ok(resp) = resp else { println!("account {n} {query:14} network error"); break };
                let status = resp.status().as_u16();
                if status == 429 { println!("account {n} {query:14} rate limited: stopping"); return; }
                let raw: Value = resp.json().await.unwrap_or(Value::Null);
                let evidence = serde_json::to_value(capture(Endpoint::ClaudeUsage, status, &raw)).unwrap();
                let reset_fields: Vec<String> = evidence["fields"].as_array().unwrap().iter()
                    .filter(|p| p["state"] != "missing" && (matches!(p["parent"].as_str(), Some("cedar_ember" | "grants" | "juniper_tide"))
                        || matches!(p["field"].as_str(), Some("cedar_ember" | "juniper_tide" | "seven_day_fable"))))
                    .map(|p| format!("{}{}{}={}{}{}{}", p["parent"].as_str().map(|s| format!("{s}.")).unwrap_or_default(),
                        p["field"].as_str().unwrap(), p["index"].as_u64().map(|i| format!("[{i}]")).unwrap_or_default(),
                        p["state"].as_str().unwrap(), p["number"].as_f64().map(|v| format!(" {v}")).unwrap_or_default(),
                        p["boolean"].as_bool().map(|v| format!(" {v}")).unwrap_or_default(),
                        p["known"].as_str().or(p["timestamp"].as_str()).map(|v| format!(" {v}")).unwrap_or_default()))
                    .collect();
                let windows: Vec<&str> = ["five_hour", "seven_day", "seven_day_opus", "seven_day_sonnet", "seven_day_fable", "seven_day_omelette",
                    "seven_day_overage_included", "seven_day_cowork", "seven_day_oauth_apps"].into_iter().filter(|k| raw[*k].is_object()).collect();
                println!("account {n} {query:14} status {status}; windows {windows:?}; unrecognized {}; {}", evidence["unrecognized_window_count"], reset_fields.join(", "));
                for r in resets(&raw) {
                    println!("    reset {} x{} usable {} until {:?} titled {}", r.kind, r.count, r.usable, r.expires_at, r.title.is_some());
                }
                // Everything else the evidence holds (windows, extra usage), times to the minute:
                // whether asking for the reset status changes any of it.
                let mut other: Vec<Value> = evidence["fields"].as_array().unwrap().iter()
                    .filter(|p| !matches!(p["parent"].as_str(), Some("cedar_ember" | "grants" | "juniper_tide"))
                        && !matches!(p["field"].as_str(), Some("cedar_ember" | "juniper_tide")))
                    .cloned().collect();
                for p in &mut other {
                    if let Some(t) = p["timestamp"].as_str().map(|t| t.chars().take(16).collect::<String>()) { p["timestamp"] = Value::String(t); }
                }
                rest.push(other);
            }
            if rest.len() < 2 { continue; }
            println!("    the rest of the payload {} with ?cedar_ember=1 ({} fields)", if rest[0] == rest[1] { "is the same" } else { "DIFFERS" }, rest[0].len());
            if rest[0].len() != rest[1].len() { println!("    field counts {} and {}", rest[0].len(), rest[1].len()); }
            for (a, b) in rest[0].iter().zip(&rest[1]).filter(|(a, b)| a != b) {
                println!("    {}.{}: {} / {} -> {} / {}", a["parent"], a["field"], a["number"], a["timestamp"], b["number"], b["timestamp"]);
            }
        }
    }
}
