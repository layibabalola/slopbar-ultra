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
    use crate::source_guard::{find, function, has, lost, places, tokens, update, words};

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

    /// Where each guard's needles are, for its failure (see `source_guard`).
    const PINS: &str = "ultra_release::tests::ultra_builds_pin_the_fork_updater_in_code (src-tauri/src/ultra_release.rs)";
    const ROTATION: &str = "ultra_release::tests::ultra_updates_wait_for_token_rotation (src-tauri/src/ultra_release.rs)";

    /// The pins `updater_config_is_the_ultra_channel` checks in tauri.conf.json are also in code, so
    /// a merged config change cannot point Ultra builds back at upstream.
    #[test]
    fn ultra_builds_pin_the_fork_updater_in_code() {
        let code = tokens(include_str!("updates.rs"));
        let updater = function(&code, "updates.rs", "fn updater(", PINS);
        for pin in [
            "#[cfg(feature = \"ultra-updates\")]",
            ".endpoints(vec![crate::ultra_release::UPDATE_ENDPOINT",
            ".pubkey(crate::ultra_release::PUBKEY)",
            "crate::ultra_release::is_ultra_version(&remote.version.to_string()) && remote.version > current",
        ] {
            assert!(has(updater, pin), "{}\n{updater}", lost("updater() in updates.rs", pin, PINS));
        }
        let every = "#[cfg(feature = \"ultra-updates\")]\nconst EVERY: Duration = crate::ultra_release::CHECK_EVERY;";
        assert!(has(&code, every), "Ultra builds check every CHECK_EVERY. {}", lost("updates.rs", every, PINS));
    }

    /// Installs wait for token rotation as named, bounded steps: an open sign-in defers the update
    /// before the download and again before the handover, and a refresh in progress is waited for
    /// within its limit. The refresh lock is an owned guard, bound once and then only handed, with
    /// the turn, to the handover, whose thread holds it until it ends; the exit cleanup runs before
    /// the installer ends SlopBar.
    #[test]
    fn ultra_updates_wait_for_token_rotation() {
        let code = tokens(include_str!("updates.rs"));
        let updater = function(&code, "updates.rs", "fn updater(", ROTATION);
        let exit = ".on_before_exit(move || crate::app::on_exit(&h))";
        assert!(has(updater, exit), "{}\n{updater}", lost("updater() in updates.rs", exit, ROTATION));
        let install = function(&code, "updates.rs", "async fn install_now(", ROTATION);
        let at = |needle: &str| find(install, needle).unwrap_or_else(|| panic!("{}\n{install}", lost("install_now in updates.rs", needle, ROTATION)));
        let deferral = "if crate::signin::any_active() {\n        return Ok(Outcome::Deferred(SIGN_IN_OPEN.into()));";
        let deferrals = places(install, deferral);
        assert_eq!(deferrals.len(), 2, "an open sign-in defers the update twice, at `{deferral}`. {}\n{install}", update(ROTATION));
        let (first, second) = (deferrals[0], deferrals[1]);
        let (download, refresh, handover) = (at("update.download("), at("let refresh = steps"), at("update_check::hand_over("));
        assert!(first < download && download < refresh && refresh < second && second < handover,
            "a sign-in defers before the download and again before the handover, after the refresh lock. {}\n{install}", update(ROTATION));
        let wait = ".run(\"Waiting for a refresh to finish\", update_check::REFRESH_WAIT_LIMIT, async move { Ok::<_, String>(refresh_lock.lock_owned().await) })";
        assert!(has(install, wait), "an owned guard, within its limit. {}\n{install}", lost("install_now in updates.rs", wait, ROTATION));
        let after = &install[refresh..];
        let after = &after[find(after, ".await?;").unwrap_or_else(|| panic!("{}", lost("The refresh lock's binding in updates.rs", ".await?;", ROTATION))) + ".await?;".len()..];
        assert_eq!(words(after, "refresh"), 1, "once bound, the guard is used only to hand it to the handover. {}\n{after}", update(ROTATION));
        let held = "update_check::hand_over(steps, update_check::HANDOFF_LIMIT, (turn, refresh), ";
        assert_eq!(find(&install[handover..], held), Some(0), "the handover's thread holds the turn and the refresh lock. {}\n{install}", lost("The handover in updates.rs", held, ROTATION));
    }

    // The deadlines' tripwire on tauri-plugin-updater's version is update_check's
    // `the_updater_plugin_is_the_version_reviewed`, which every tree has.
}
