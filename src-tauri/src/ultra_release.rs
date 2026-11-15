//! SlopBar Ultra's own update channel. Ultra release builds (`ultra-updates`) take updates only
//! from the fork's releases, signed with the fork's key, so an Ultra install never "updates" to
//! upstream SlopBar. See docs/ULTRA-RELEASES.md.

/// A fixed release (not `releases/latest`) so no other release on the fork can become the update.
#[cfg_attr(not(feature = "ultra-updates"), allow(dead_code))]
pub const UPDATE_ENDPOINT: &str =
    "https://github.com/layibabalola/slopbar-ultra/releases/download/ultra-latest/latest.json";

/// The fork's minisign public key (key id 593D8A8B75A9A204). The private key never enters the repo.
#[cfg_attr(not(feature = "ultra-updates"), allow(dead_code))]
pub const PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDU5M0Q4QThCNzVBOUEyMDQKUldRRW9xbDFpNG85V1FlWnB3SUlyYjVqa1JvaFJQSll2SmxGeUVaajZBRktlZSsva2grVUJKSjkK";

/// How often Ultra release builds look for an update after the first check; upstream builds look
/// every 6 hours. Every push to `ultra` that changes what ships is published by itself, so a fix
/// reaches every machine within a quarter of an hour of its release.
#[cfg_attr(not(feature = "ultra-updates"), allow(dead_code))]
pub const CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Ultra versions are `<package version>-ultra.<build>`; anything else is not an Ultra release.
#[cfg_attr(not(feature = "ultra-updates"), allow(dead_code))]
pub fn is_ultra_version(version: &str) -> bool {
    let Some((base, build)) = version.split_once("-ultra.") else { return false };
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    base.split('.').count() == 3 && base.split('.').all(numeric) && numeric(build)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPSTREAM_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEE0MDdFRDM5QzIxRTZGMTEKUldRUmJ4N0NPZTBIcEcvT2F1YThXMTB0UVRRREpMUmtPSjJUS1V4M2poTzdFUXBtejB0V1FNUFAK";

    #[test]
    fn updater_config_is_the_ultra_channel() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let updater = &conf["plugins"]["updater"];
        assert_eq!(updater["pubkey"], PUBKEY);
        assert_eq!(updater["endpoints"], serde_json::json!([UPDATE_ENDPOINT]));
        assert_ne!(PUBKEY, UPSTREAM_PUBKEY);
        assert!(!updater.to_string().contains("thelioo"));
    }

    #[test]
    fn ultra_builds_check_every_quarter_hour() {
        // The owner asked for checks every 15 to 30 minutes instead of every 6 hours.
        assert_eq!(CHECK_EVERY, std::time::Duration::from_secs(15 * 60));
    }

    #[test]
    fn only_ultra_versions_count() {
        assert!(is_ultra_version("0.1.0-ultra.1"));
        assert!(is_ultra_version("0.2.12-ultra.30"));
        assert!(!is_ultra_version("0.2.57"));
        assert!(!is_ultra_version("0.1.0"));
        assert!(!is_ultra_version("0.1.0-ultra."));
        assert!(!is_ultra_version("0.1.0-ultra.1.2"));
        assert!(!is_ultra_version("0.1-ultra.1"));
    }

    /// updates.rs without whole-line comments (a line commented out does not count), and the part
    /// from `signature` to the first line that is `}`.
    fn updates_rs(signature: &str) -> (String, String) {
        let code = include_str!("updates.rs").lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
        let start = code.find(signature).unwrap_or_else(|| panic!("updates.rs has no `{signature}`"));
        let end = code[start..].find("\n}").map_or(code.len(), |e| start + e + 2);
        let body = code[start..end].to_string();
        (code, body)
    }

    /// The pins `updater_config_is_the_ultra_channel` checks in tauri.conf.json are also in code, so
    /// a merged config change cannot point Ultra builds back at upstream.
    #[test]
    fn ultra_builds_pin_the_fork_updater_in_code() {
        let (code, updater) = updates_rs("fn updater(");
        for pin in [
            "#[cfg(feature = \"ultra-updates\")]",
            ".endpoints(vec![crate::ultra_release::UPDATE_ENDPOINT",
            ".pubkey(crate::ultra_release::PUBKEY)",
            "crate::ultra_release::is_ultra_version(&remote.version.to_string()) && remote.version > current",
        ] {
            assert!(updater.contains(pin), "updater() lost `{pin}`:\n{updater}");
        }
        assert!(code.contains("#[cfg(feature = \"ultra-updates\")]\nconst EVERY: Duration = crate::ultra_release::CHECK_EVERY;"), "Ultra builds check every CHECK_EVERY");
    }

    /// Installs wait for token rotation as named, bounded steps: an open sign-in defers the update
    /// before the download and again before the handover, and a refresh in progress is waited for
    /// within its limit. The refresh lock is an owned guard, bound once and then only handed, with
    /// the turn, to the handover, whose thread holds it until it ends; the exit cleanup runs before
    /// the installer ends SlopBar.
    #[test]
    fn ultra_updates_wait_for_token_rotation() {
        let (_, updater) = updates_rs("fn updater(");
        assert!(updater.contains(".on_before_exit(move || crate::app::on_exit(&h))"), "{updater}");
        let (_, install) = updates_rs("async fn install_now(");
        let at = |needle: &str| install.find(needle).unwrap_or_else(|| panic!("install_now lost `{needle}`:\n{install}"));
        let deferral = "if crate::signin::any_active() {\n        return Ok(Outcome::Deferred(SIGN_IN_OPEN.into()));";
        assert_eq!(install.matches(deferral).count(), 2, "{install}");
        let (first, second) = (at(deferral), install.rfind(deferral).unwrap());
        let (refresh, handover) = (at("let refresh = steps"), at("update_check::hand_over("));
        assert!(first < at("update.download(") && at("update.download(") < refresh && refresh < second && second < handover,
            "a sign-in defers before the download and again before the handover, after the refresh lock:\n{install}");
        assert!(install.contains(".run(\"Waiting for a refresh to finish\", update_check::REFRESH_WAIT_LIMIT, async move { Ok::<_, String>(refresh_lock.lock_owned().await) })"),
            "an owned guard, within its limit:\n{install}");
        let after = &install[refresh..];
        let after = &after[after.find(".await?;").expect("the guard's binding ends") + ".await?;".len()..];
        let named = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        let uses = after.match_indices("refresh").filter(|(i, w)| !named(after[..*i].chars().next_back()) && !named(after[i + w.len()..].chars().next())).count();
        assert_eq!(uses, 1, "once bound, the guard is used only to hand it to the handover:\n{after}");
        assert!(install[handover..].starts_with("update_check::hand_over(steps, update_check::HANDOFF_LIMIT, (turn, refresh), "),
            "the handover's thread holds the turn and the refresh lock:\n{install}");
    }

    /// The deadlines in updates.rs rely on tauri-plugin-updater 2.12.0's internals (updater.rs: the
    /// builder's `timeout` reaching only the check; `check()` carrying the `configure_client` hook
    /// into the `Update` it returns, with no timeout of its own; `Update::download` applying both).
    /// A different version must be read again before this test is changed.
    #[test]
    fn the_updater_plugin_is_the_version_reviewed() {
        let version = include_str!("../Cargo.lock")
            .split("[[package]]")
            .find(|package| package.lines().any(|l| l == "name = \"tauri-plugin-updater\""))
            .and_then(|package| package.lines().find_map(|l| l.strip_prefix("version = \"")?.strip_suffix('"')));
        assert_eq!(version, Some("2.12.0"), "re-review the deadlines in src-tauri/src/updates.rs against this tauri-plugin-updater");
    }
}
