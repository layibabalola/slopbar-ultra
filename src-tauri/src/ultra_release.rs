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
}
