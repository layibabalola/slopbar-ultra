//! In-process usage evidence. Provider strings, identities, bodies and headers cannot
//! be represented by this schema. Only explicitly named fields are inspected.
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Endpoint {
    ClaudeProfile,
    ClaudeUsage,
    CodexUsage,
    CodexResetDetails,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    AccountMatches,
    OrganizationMatches,
    EmailMatches,
    FiveHour,
    SevenDay,
    SevenDayOpus,
    SevenDaySonnet,
    SevenDayFable,
    Utilization,
    ResetsAt,
    PrimaryWindow,
    SecondaryWindow,
    UsedPercent,
    WindowSeconds,
    ResetAt,
    CedarEmber,
    Grants,
    ResetsLeft,
    Paused,
    UsableNow,
    EndsAt,
    JuniperTide,
    Available,
    Eligible,
    NextAvailableAt,
    ResetCredits,
    AvailableCount,
    ApplicableAvailableCount,
    Credits,
    HasCredits,
    Unlimited,
    Balance,
    Status,
    ResetType,
    ExpiresAt,
    ExtraUsage,
    IsEnabled,
    MonthlyLimit,
    UsedCredits,
    // Claude's reset status (`cedar_ember`, read with `?cedar_ember=1`) and its grants.
    IneligibleReason,
    AtLimit,
    Exhausted,
    ExhaustedLimit,
    NextGrantId,
    WeeklyResetsAt,
    CooldownUntil,
    Label,
    ResetsTotal,
    StartsAt,
    Clears,
    Clear,
    Blocking,
    Block,
    UseRequiresLimit,
    PercentUsed,
    GivesBackWeekly,
    UnavailableReason,
    /// The client the provider took the request for (`event_props.surface`).
    EventSurface,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Missing,
    Null,
    Parsed,
    Invalid,
}
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Known {
    Available,
    Consumed,
    Expired,
    Session,
    Weekly,
    Full,
    Unknown,
    // Claude's limit names (a reset's `clears`, `blocking`, `percent_used`; the status's
    // `exhausted`), and its reasons a reset is not offered or cannot be used now.
    Fable,
    SevenDayOverageIncluded,
    SevenDayOpus,
    SevenDaySonnet,
    SevenDayFable,
    SevenDayCowork,
    SevenDayOmelette,
    SevenDayOauthApps,
    ConfigOff,
    Tier,
    Seat,
    Mobile,
    Surface,
    CliVersion,
    NoGrant,
    Tenure,
    OtherExperiment,
    Unavailable,
    Cooldown,
    BlockedFiveHour,
    BlockedSevenDay,
    BlockedOpus,
    BlockedSonnet,
    BlockedOther,
    CannotClear,
    NeedsLimit,
    ClaudeAi,
    ClaudeCodeCli,
}
// Empty parts are left out when written (a reader takes a missing one as empty), so that a
// reading's evidence stays small in the diagnostics journal.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Probe {
    field: Field,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<Field>,
    #[serde(skip_serializing_if = "Option::is_none")]
    index: Option<u16>,
    state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    number: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    boolean: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    known: Option<Known>,
}

/// One reading's evidence holds at most this many probes (then `truncated`): the journal keeps
/// 10,000 events within 16 MiB.
const MAX_PROBES: usize = 256;
/// Claude reset grants, and items of each list in one, that are probed.
const MAX_GRANT_PROBES: usize = 8;
const MAX_LIST_PROBES: usize = 4;
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    pub endpoint: Endpoint,
    pub status: Option<u16>,
    pub parsed_json: bool,
    fields: Vec<Probe>,
    truncated: bool,
    unrecognized_window_count: u16,
}
pub type Observer<'a> = dyn Fn(Evidence) + Send + Sync + 'a;
enum Parse {
    Object,
    Array,
    Percent,
    Count,
    Bool,
    Time,
    Epoch,
    Balance,
    Known,
    Decimal,
    /// A non-empty string, recorded only as present: never its text.
    Present,
}
impl Evidence {
    /// A Claude reading that carried the reset status (asked with `?cedar_ember=1`): its evidence
    /// is compared only with other such readings (diagnostics' repeat suppression), as the plain
    /// readings between them say less.
    pub fn carries_reset_status(&self) -> bool {
        self.fields.iter().any(|p| p.field == Field::CedarEmber && p.parent.is_none() && p.state == State::Parsed)
    }

    pub fn outcome(endpoint: Endpoint, status: Option<u16>) -> Self {
        Self {
            endpoint,
            status,
            parsed_json: false,
            fields: vec![],
            truncated: false,
            unrecognized_window_count: 0,
        }
    }
    fn probe(
        &mut self,
        field: Field,
        parent: Option<Field>,
        index: Option<u16>,
        value: Option<&Value>,
        parse: Parse,
    ) {
        if self.fields.len() >= MAX_PROBES {
            self.truncated = true;
            return;
        }
        let mut p = Probe {
            field,
            parent,
            index,
            state: State::Missing,
            number: None,
            boolean: None,
            timestamp: None,
            known: None,
        };
        if let Some(v) = value {
            p.state = if v.is_null() {
                State::Null
            } else {
                State::Invalid
            };
            if !v.is_null() {
                match parse {
                    Parse::Object if v.is_object() => p.state = State::Parsed,
                    Parse::Array if v.is_array() => {
                        p.number = Some(v.as_array().unwrap().len() as f64);
                        p.state = State::Parsed;
                    }
                    Parse::Percent => {
                        p.number = v
                            .as_f64()
                            .filter(|n| n.is_finite() && (0.0..=100.0).contains(n))
                    }
                    Parse::Count => {
                        p.number = v
                            .as_u64()
                            .filter(|n| *n <= u32::MAX as u64)
                            .map(|n| n as f64)
                    }
                    Parse::Bool => p.boolean = v.as_bool(),
                    Parse::Time => {
                        p.timestamp = v
                            .as_str()
                            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                            .map(|t| t.with_timezone(&Utc))
                    }
                    Parse::Epoch => {
                        p.timestamp = v.as_i64().and_then(|n| Utc.timestamp_opt(n, 0).single())
                    }
                    Parse::Balance => {
                        p.number = v
                            .as_str()
                            .and_then(|s| s.parse::<f64>().ok())
                            .filter(|n| n.is_finite())
                    }
                    Parse::Decimal => p.number = v.as_f64().filter(|n| n.is_finite()),
                    Parse::Known => p.known = Some(known(v.as_str())),
                    Parse::Present if v.as_str().is_some_and(|s| !s.is_empty()) => p.state = State::Parsed,
                    _ => {}
                }
                if p.number.is_some()
                    || p.boolean.is_some()
                    || p.timestamp.is_some()
                    || p.known.is_some_and(|k| k != Known::Unknown)
                {
                    p.state = State::Parsed;
                }
            }
        }
        self.fields.push(p);
    }
}
fn get<'a>(v: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    match v.get(snake) {
        Some(x) if !x.is_null() => Some(x),
        x => v.get(camel).or(x),
    }
}

/// A provider enum string as one of the names this schema knows; any other text is `Unknown`.
fn known(s: Option<&str>) -> Known {
    use Known::*;
    match s {
        Some("available") => Available,
        Some("consumed" | "used") => Consumed,
        Some("expired") => Expired,
        Some("session" | "five_hour") => Session,
        Some("weekly" | "seven_day") => Weekly,
        Some("full") => Full,
        Some("fable") => Fable,
        Some("seven_day_overage_included") => SevenDayOverageIncluded,
        Some("seven_day_opus") => SevenDayOpus,
        Some("seven_day_sonnet") => SevenDaySonnet,
        Some("seven_day_fable") => SevenDayFable,
        Some("seven_day_cowork") => SevenDayCowork,
        Some("seven_day_omelette") => SevenDayOmelette,
        Some("seven_day_oauth_apps") => SevenDayOauthApps,
        Some("config_off") => ConfigOff,
        Some("tier") => Tier,
        Some("seat") => Seat,
        Some("mobile") => Mobile,
        Some("surface") => Surface,
        Some("cli_version") => CliVersion,
        Some("no_grant") => NoGrant,
        Some("tenure") => Tenure,
        Some("other_experiment") => OtherExperiment,
        Some("unavailable") => Unavailable,
        Some("cooldown") => Cooldown,
        Some("blocked_five_hour") => BlockedFiveHour,
        Some("blocked_seven_day") => BlockedSevenDay,
        Some("blocked_opus") => BlockedOpus,
        Some("blocked_sonnet") => BlockedSonnet,
        Some("blocked_other") => BlockedOther,
        Some("cannot_clear") => CannotClear,
        Some("needs_limit") => NeedsLimit,
        Some("claude_ai") => ClaudeAi,
        Some("claude_code_cli") => ClaudeCodeCli,
        _ => Unknown,
    }
}

/// Up to `MAX_LIST_PROBES` items of a list of limit names, one probe each under `item` (index: the
/// grant's).
fn limits(e: &mut Evidence, list: Option<&Value>, item: Field, parent: Field, index: Option<u16>) {
    if let Some(items) = list.and_then(Value::as_array) {
        e.truncated |= items.len() > MAX_LIST_PROBES;
        for v in items.iter().take(MAX_LIST_PROBES) {
            e.probe(item, Some(parent), index, Some(v), Parse::Known);
        }
    }
}
pub fn capture(endpoint: Endpoint, status: u16, raw: &Value) -> Evidence {
    use Field::*;
    let mut e = Evidence::outcome(endpoint, Some(status));
    e.parsed_json = true;
    let windows: Vec<(Field, &str, Option<&Value>)> = match endpoint {
        Endpoint::ClaudeUsage => [
            (FiveHour, "five_hour"),
            (SevenDay, "seven_day"),
            (SevenDayOpus, "seven_day_opus"),
            (SevenDaySonnet, "seven_day_sonnet"),
            (SevenDayFable, "seven_day_fable"),
        ]
        .into_iter()
        .map(|(f, key)| (f, key, raw.get(key)))
        .collect(),
        Endpoint::CodexUsage => [
            (PrimaryWindow, "primary_window"),
            (SecondaryWindow, "secondary_window"),
        ]
        .into_iter()
        .map(|(f, key)| (f, key, raw.get("rate_limit").and_then(|v| v.get(key))))
        .collect(),
        Endpoint::CodexResetDetails | Endpoint::ClaudeProfile => vec![],
    };
    for (f, _, w) in &windows {
        e.probe(*f, None, None, *w, Parse::Object);
        if let Some(w) = w.filter(|w| w.is_object()) {
            if endpoint == Endpoint::ClaudeUsage {
                e.probe(
                    Utilization,
                    Some(*f),
                    None,
                    w.get("utilization"),
                    Parse::Percent,
                );
                e.probe(ResetsAt, Some(*f), None, w.get("resets_at"), Parse::Time);
            } else {
                e.probe(
                    UsedPercent,
                    Some(*f),
                    None,
                    w.get("used_percent"),
                    Parse::Percent,
                );
                e.probe(
                    WindowSeconds,
                    Some(*f),
                    None,
                    w.get("limit_window_seconds"),
                    Parse::Count,
                );
                e.probe(ResetAt, Some(*f), None, w.get("reset_at"), Parse::Epoch);
            }
        }
    }
    if endpoint == Endpoint::ClaudeUsage {
        e.unrecognized_window_count = raw
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(k, v)| {
                        k.as_str() != "extra_usage"
                            && !windows.iter().any(|(_, key, _)| key == k)
                            && v.is_object()
                            && v.get("utilization").is_some()
                    })
                    .count()
                    .min(u16::MAX as usize) as u16
            })
            .unwrap_or(0);
        for (f, key) in [(CedarEmber, "cedar_ember"), (JuniperTide, "juniper_tide")] {
            e.probe(f, None, None, raw.get(key), Parse::Object);
        }
        let cedar = &raw["cedar_ember"];
        e.probe(
            Grants,
            Some(CedarEmber),
            None,
            cedar.get("grants"),
            Parse::Array,
        );
        for (f, key, parse) in [
            (Eligible, "eligible", Parse::Bool),
            (IneligibleReason, "ineligible_reason", Parse::Known),
            (AtLimit, "at_limit", Parse::Bool),
            (Exhausted, "exhausted", Parse::Array),
            (NextGrantId, "next_grant_id", Parse::Present),
            (WeeklyResetsAt, "weekly_resets_at", Parse::Time),
            (CooldownUntil, "cooldown_until", Parse::Time),
        ] {
            if cedar.is_object() { e.probe(f, Some(CedarEmber), None, cedar.get(key), parse); }
        }
        limits(&mut e, cedar.get("exhausted"), ExhaustedLimit, CedarEmber, None);
        if cedar.is_object() {
            e.probe(EventSurface, Some(CedarEmber), None, cedar.get("event_props").and_then(|p| p.get("surface")), Parse::Known);
        }
        if let Some(grants) = cedar.get("grants").and_then(Value::as_array) {
            e.truncated |= grants.len() > MAX_GRANT_PROBES;
            for (i, g) in grants.iter().take(MAX_GRANT_PROBES).enumerate() {
                let index = Some(i as u16);
                for (f, snake, camel, parse) in [
                    (ResetsLeft, "resets_left", "resetsLeft", Parse::Count),
                    (Paused, "paused", "paused", Parse::Bool),
                    (UsableNow, "usable_now", "usableNow", Parse::Bool),
                    (EndsAt, "ends_at", "endsAt", Parse::Time),
                    (Label, "label", "label", Parse::Present),
                    (ResetsTotal, "resets_total", "resetsTotal", Parse::Count),
                    (StartsAt, "starts_at", "startsAt", Parse::Time),
                    (Clears, "clears", "clears", Parse::Array),
                    (Blocking, "blocking", "blocking", Parse::Array),
                    (UseRequiresLimit, "use_requires_limit", "useRequiresLimit", Parse::Bool),
                    (PercentUsed, "percent_used", "percentUsed", Parse::Object),
                    (ResetType, "type", "type", Parse::Known),
                    (GivesBackWeekly, "gives_back_weekly", "givesBackWeekly", Parse::Bool),
                    (UnavailableReason, "unavailable_reason", "unavailableReason", Parse::Known),
                ] {
                    e.probe(f, Some(Grants), index, get(g, snake, camel), parse);
                }
                limits(&mut e, get(g, "clears", "clears"), Clear, Grants, index);
                limits(&mut e, get(g, "blocking", "blocking"), Block, Grants, index);
            }
        }
        let j = &raw["juniper_tide"];
        for (f, snake, camel, parse) in [
            (Available, "available", "available", Parse::Bool),
            (Eligible, "eligible", "eligible", Parse::Bool),
            (
                NextAvailableAt,
                "next_available_at",
                "nextAvailableAt",
                Parse::Time,
            ),
        ] {
            e.probe(f, Some(JuniperTide), None, get(j, snake, camel), parse);
        }
        e.probe(
            ExtraUsage,
            None,
            None,
            raw.get("extra_usage"),
            Parse::Object,
        );
        let credits = &raw["extra_usage"];
        for (f, key, parse) in [
            (IsEnabled, "is_enabled", Parse::Bool),
            (MonthlyLimit, "monthly_limit", Parse::Decimal),
            (UsedCredits, "used_credits", Parse::Decimal),
            (Utilization, "utilization", Parse::Percent),
        ] {
            e.probe(f, Some(ExtraUsage), None, credits.get(key), parse);
        }
    } else if endpoint == Endpoint::CodexUsage {
        e.probe(
            ResetCredits,
            None,
            None,
            raw.get("rate_limit_reset_credits"),
            Parse::Object,
        );
        let r = &raw["rate_limit_reset_credits"];
        for (f, key) in [
            (AvailableCount, "available_count"),
            (ApplicableAvailableCount, "applicable_available_count"),
        ] {
            e.probe(f, Some(ResetCredits), None, r.get(key), Parse::Count);
        }
        e.probe(Credits, None, None, raw.get("credits"), Parse::Object);
        let c = &raw["credits"];
        for (f, key, parse) in [
            (HasCredits, "has_credits", Parse::Bool),
            (Unlimited, "unlimited", Parse::Bool),
            (Balance, "balance", Parse::Balance),
        ] {
            e.probe(f, Some(Credits), None, c.get(key), parse);
        }
    } else {
        e.probe(Credits, None, None, raw.get("credits"), Parse::Array);
        if let Some(credits) = raw.get("credits").and_then(Value::as_array) {
            e.truncated |= credits.len() > 64;
            for (i, c) in credits.iter().take(64).enumerate() {
                for (f, key, parse) in [
                    (Status, "status", Parse::Known),
                    (ResetType, "reset_type", Parse::Known),
                    (ExpiresAt, "expires_at", Parse::Time),
                ] {
                    e.probe(f, Some(Credits), Some(i as u16), c.get(key), parse);
                }
            }
        }
    }
    e
}

/// Compare identities only in memory; the diagnostic schema can hold booleans,
/// never an email, organization name, UUID, profile body or token.
pub fn profile(status: u16, raw: &Value, slot: &crate::accounts::Slot) -> Evidence {
    let mut e = Evidence::outcome(Endpoint::ClaudeProfile, Some(status));
    e.parsed_json = true;
    let expected = &slot.data["oauth_account"];
    for (field, actual, expected) in [
        (Field::AccountMatches, raw["account"]["uuid"].as_str(), expected["accountUuid"].as_str()),
        (Field::OrganizationMatches, raw["organization"]["uuid"].as_str(), expected["organizationUuid"].as_str()),
        (Field::EmailMatches, raw["account"]["email"].as_str(), slot.email.as_deref()),
    ] {
        let comparison = actual.zip(expected).filter(|(a, b)| !a.is_empty() && !b.is_empty())
            .map(|(a, b)| Value::Bool(a == b));
        e.probe(field, None, None, comparison.as_ref(), Parse::Bool);
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extra_usage_is_credit_utilization_not_an_unrecognized_included_window() {
        let fixture: Value = serde_json::from_str(include_str!("fixtures/usage-divergence.json")).unwrap();
        let e = capture(Endpoint::ClaudeUsage, 200, &fixture["claude_usage"]);
        assert_eq!(e.unrecognized_window_count, 0);
        assert!(e.fields.iter().any(|p| p.field == Field::SevenDayFable && p.state == State::Missing));
        assert!(e.fields.iter().any(|p| p.field == Field::Utilization && p.parent == Some(Field::ExtraUsage) && p.number == Some(18.3675)));
    }

    /// A full reset status (synthetic) with text in every place a provider could put it: none of it
    /// reaches the evidence, only what each field is and the names this schema knows.
    #[test]
    fn a_reset_status_leaves_no_provider_text_in_the_evidence() {
        let grant = |i: usize| serde_json::json!({ "id": format!("SECRET-grant-{i}"), "label": "SECRET-label Fable reset",
            "resets_total": 2, "resets_left": 1, "starts_at": "2026-11-20T00:00:00Z", "ends_at": "2026-12-01T00:00:00Z",
            "clears": ["seven_day", "SECRET-limit", "five_hour", "seven_day_opus", "seven_day_sonnet", "seven_day_cowork"],
            "blocking": ["SECRET-block"], "paused": false, "usable_now": true, "use_requires_limit": true,
            "percent_used": { "SECRET-key": 40, "seven_day": 57 }, "type": "SECRET-type", "gives_back_weekly": true,
            "unavailable_reason": "SECRET-reason" });
        let raw = serde_json::json!({
            "seven_day": { "utilization": 57, "resets_at": "2026-11-27T06:00:00Z" },
            "cedar_ember": { "eligible": false, "ineligible_reason": "surface", "at_limit": false, "exhausted": ["SECRET-exhausted", "seven_day"],
                "grants": (0..12).map(grant).collect::<Vec<_>>(), "next_grant_id": "SECRET-next", "weekly_resets_at": "2026-11-27T06:00:00Z",
                "cooldown_until": "SECRET-time", "event_props": { "surface": "SECRET-surface", "tier": "SECRET-tier" } },
        });
        let e = capture(Endpoint::ClaudeUsage, 200, &raw);
        let text = serde_json::to_string(&e).unwrap();
        assert!(!text.contains("SECRET"), "{text}");
        let probe = |field: Field, parent: Option<Field>| e.fields.iter().find(|p| p.field == field && p.parent == parent).unwrap();
        assert!(probe(Field::IneligibleReason, Some(Field::CedarEmber)).known == Some(Known::Surface));
        assert!(probe(Field::NextGrantId, Some(Field::CedarEmber)).state == State::Parsed);
        assert!(probe(Field::CooldownUntil, Some(Field::CedarEmber)).state == State::Invalid);
        assert!(probe(Field::EventSurface, Some(Field::CedarEmber)).known == Some(Known::Unknown));
        assert!(probe(Field::GivesBackWeekly, Some(Field::Grants)).boolean == Some(true));
        assert!(probe(Field::ResetType, Some(Field::Grants)).state == State::Invalid);
        assert!(e.fields.iter().any(|p| p.field == Field::Clear && p.known == Some(Known::Unknown)));
        // At most 8 grants and 4 items of a list are probed, and that is said.
        assert!(e.truncated);
        assert_eq!(e.fields.iter().filter(|p| p.field == Field::Label).count(), 8);
        assert_eq!(e.fields.iter().filter(|p| p.field == Field::Clear && p.index == Some(0)).count(), 4);
        assert!(e.carries_reset_status() && !capture(Endpoint::ClaudeUsage, 200, &serde_json::json!({ "cedar_ember": null })).carries_reset_status());
        // Small in the journal: empty parts are not written, and the worst case stays far under 16 KB.
        assert!(!text.contains("null"), "{text}");
        assert!(text.len() < 16 * 1024, "{} bytes", text.len());
        // What an older build wrote (every part written, empty ones as null) still reads.
        let written = r#"{"endpoint":"claude_usage","status":200,"parsed_json":true,"fields":[{"field":"cedar_ember","parent":null,"index":null,"state":"null","number":null,"boolean":null,"timestamp":null,"known":null}],"truncated":false,"unrecognized_window_count":0}"#;
        assert!(serde_json::from_str::<Evidence>(written).is_ok());
        let back: Evidence = serde_json::from_str(&text).unwrap();
        assert!(back == e);
    }

    #[test]
    fn profile_reports_matching_unknown_and_mismatched_identity_without_values() {
        let mut slot = crate::test_support::slot("PRIVATE-ID", "SECRET-access", "SECRET-refresh", 0);
        slot.email = Some("PRIVATE-email".into());
        slot.data["oauth_account"] = serde_json::json!({"accountUuid":"PRIVATE-account","organizationUuid":"PRIVATE-org"});
        let raw = serde_json::json!({"account":{"uuid":"PRIVATE-account","email":"PRIVATE-email"},"organization":{"uuid":"PRIVATE-other-org"},"credentials":"SECRET"});
        let e = profile(200, &raw, &slot);
        assert!(e.fields.iter().any(|p| p.field == Field::AccountMatches && p.boolean == Some(true)));
        assert!(e.fields.iter().any(|p| p.field == Field::OrganizationMatches && p.boolean == Some(false)));
        assert!(!serde_json::to_string(&e).unwrap().contains("PRIVATE"));
        assert!(!serde_json::to_string(&e).unwrap().contains("SECRET"));
        assert!(profile(200, &Value::Null, &slot).fields.iter().all(|p| p.state == State::Missing));
    }
    #[test]
    fn capture_is_allowlisted_and_distinguishes_absent_null_and_invalid() {
        let raw = serde_json::json!({"identity":"PRIVATE-CANARY", "authorization":"SECRET-CANARY", "cedar_ember":null,
            "juniper_tide":{"eligible":"SECRET-CANARY"}, "five_hour":{"utilization":10,"resets_at":"2026-10-13T05:00:00Z"},
            "seven_day_fable":{"utilization":3,"resets_at":"2026-10-19T04:00:00Z"}, "private_window":{"utilization":9}});
        let e = capture(Endpoint::ClaudeUsage, 200, &raw);
        let text = serde_json::to_string(&e).unwrap();
        for secret in [
            "PRIVATE-CANARY",
            "SECRET-CANARY",
            "private_window",
            "authorization",
            "identity",
        ] {
            assert!(!text.contains(secret));
        }
        assert!(e
            .fields
            .iter()
            .any(|p| p.field == Field::SevenDay && p.state == State::Missing));
        assert!(e
            .fields
            .iter()
            .any(|p| p.field == Field::CedarEmber && p.state == State::Null));
        assert!(e
            .fields
            .iter()
            .any(|p| p.field == Field::Eligible && p.state == State::Invalid));
        assert_eq!(e.unrecognized_window_count, 1);
    }
    #[test]
    fn detail_titles_and_unknown_enums_never_escape() {
        let e = capture(
            Endpoint::CodexResetDetails,
            200,
            &serde_json::json!({"credits":[{"title":"SECRET", "reset_type":"SECRET", "status":"available", "expires_at":"bad SECRET"}]}),
        );
        assert!(!serde_json::to_string(&e).unwrap().contains("SECRET"));
        assert!(e
            .fields
            .iter()
            .any(|p| p.field == Field::ResetType && p.known == Some(Known::Unknown)));
        assert!(e
            .fields
            .iter()
            .any(|p| p.field == Field::ExpiresAt && p.state == State::Invalid));
    }
}
