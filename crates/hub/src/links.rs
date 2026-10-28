//! Deep links and app-site association (M2-010).
//!
//! - `GET /t/{thread_id}` redirects to the mobile web page's thread, `/#/t/{thread_id}` (the page
//!   is served from Workers Static Assets, plan §4.2).
//! - `GET /.well-known/apple-app-site-association` and `GET /.well-known/assetlinks.json` tell iOS
//!   and Android which installed app opens `/t/*` links. Both documents come from config vars
//!   (`[vars]` in wrangler.toml, or Worker vars). An empty or invalid config is `404`: nothing is
//!   claimed for an app that is not configured.
//!
//! The documents are pure functions of [`LinkConfig`] (tested natively); `links/routes.rs` is the
//! wasm32 glue that reads the vars.

use protocol::turn_event::is_valid_id;
use serde_json::{Value, json};

#[cfg(target_arch = "wasm32")]
pub mod routes;

/// Var names (wrangler.toml `[vars]`).
pub const APPLE_TEAM_ID: &str = "APPLE_TEAM_ID";
pub const APPLE_BUNDLE_ID: &str = "APPLE_BUNDLE_ID";
pub const ANDROID_PACKAGE_NAME: &str = "ANDROID_PACKAGE_NAME";
pub const ANDROID_SHA256_CERT_FINGERPRINTS: &str = "ANDROID_SHA256_CERT_FINGERPRINTS";

/// The app-site association config, as the vars give it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkConfig {
    pub apple_team_id: String,
    pub apple_bundle_id: String,
    pub android_package_name: String,
    /// Comma-separated `AA:BB:...` SHA-256 certificate fingerprints.
    pub android_sha256_cert_fingerprints: String,
}

impl LinkConfig {
    /// Reads the config through `var` (a lookup by name; `None` when the var is unset).
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| var(name).unwrap_or_default();
        Self {
            apple_team_id: get(APPLE_TEAM_ID),
            apple_bundle_id: get(APPLE_BUNDLE_ID),
            android_package_name: get(ANDROID_PACKAGE_NAME),
            android_sha256_cert_fingerprints: get(ANDROID_SHA256_CERT_FINGERPRINTS),
        }
    }
}

/// The location of a thread's deep link, or `None` for an id that is not a valid id.
pub fn deep_link_location(thread_id: &str) -> Option<String> {
    is_valid_id(thread_id).then(|| format!("/#/t/{thread_id}"))
}

/// `apple-app-site-association` (Universal Links) for the configured app, or `None` when the
/// team id or bundle id is missing or malformed.
pub fn apple_app_site_association(config: &LinkConfig) -> Option<Value> {
    let team = config.apple_team_id.trim();
    let bundle = config.apple_bundle_id.trim();
    if !valid_team_id(team) || !valid_bundle_id(bundle) {
        return None;
    }
    Some(json!({
        "applinks": {
            "apps": [],
            "details": [{ "appID": format!("{team}.{bundle}"), "paths": ["/t/*"] }],
        },
    }))
}

/// `assetlinks.json` (App Links) for the configured Android app, or `None` when the package
/// name or the fingerprint list is missing or malformed.
pub fn asset_links(config: &LinkConfig) -> Option<Value> {
    let package = config.android_package_name.trim();
    if !valid_package_name(package) {
        return None;
    }
    let fingerprints: Vec<&str> = config
        .android_sha256_cert_fingerprints
        .split(',')
        .map(str::trim)
        .filter(|fingerprint| !fingerprint.is_empty())
        .collect();
    if fingerprints.is_empty() || !fingerprints.iter().all(|f| valid_fingerprint(f)) {
        return None;
    }
    Some(json!([{
        "relation": ["delegate_permission/common.handle_all_urls"],
        "target": {
            "namespace": "android_app",
            "package_name": package,
            "sha256_cert_fingerprints": fingerprints,
        },
    }]))
}

/// An Apple team id: ten letters and digits.
fn valid_team_id(text: &str) -> bool {
    text.len() == 10 && text.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// A bundle id: reverse-DNS characters, no empty label.
fn valid_bundle_id(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// An Android package name: dotted identifiers, at least one dot.
fn valid_package_name(text: &str) -> bool {
    text.contains('.')
        && text.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
}

/// A SHA-256 certificate fingerprint: 32 hex pairs joined by colons.
fn valid_fingerprint(text: &str) -> bool {
    let pairs: Vec<&str> = text.split(':').collect();
    pairs.len() == 32
        && pairs
            .iter()
            .all(|pair| pair.len() == 2 && pair.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> LinkConfig {
        LinkConfig::from_vars(|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    fn fingerprint() -> String {
        vec!["AB"; 32].join(":")
    }

    #[test]
    fn an_empty_config_serves_no_document() {
        let empty = LinkConfig::default();
        assert_eq!(apple_app_site_association(&empty), None);
        assert_eq!(asset_links(&empty), None);
    }

    #[test]
    fn the_apple_document_names_team_and_bundle_for_thread_paths() {
        let cfg = config(&[
            (APPLE_TEAM_ID, "ABCDE12345"),
            (APPLE_BUNDLE_ID, "dev.sprite.machine"),
        ]);
        assert_eq!(
            apple_app_site_association(&cfg),
            Some(json!({
                "applinks": {
                    "apps": [],
                    "details": [{ "appID": "ABCDE12345.dev.sprite.machine", "paths": ["/t/*"] }],
                },
            }))
        );
    }

    #[test]
    fn a_malformed_apple_config_serves_nothing() {
        assert_eq!(
            apple_app_site_association(&config(&[
                (APPLE_TEAM_ID, "short"),
                (APPLE_BUNDLE_ID, "a.b")
            ])),
            None
        );
        assert_eq!(
            apple_app_site_association(&config(&[(APPLE_TEAM_ID, "ABCDE12345")])),
            None,
            "the bundle id is required too"
        );
    }

    #[test]
    fn the_android_document_lists_every_fingerprint() {
        let two = format!("{}, {}", fingerprint(), fingerprint());
        let cfg = config(&[
            (ANDROID_PACKAGE_NAME, "dev.sprite.machine"),
            (ANDROID_SHA256_CERT_FINGERPRINTS, &two),
        ]);
        let document = asset_links(&cfg).expect("document");
        assert_eq!(document[0]["target"]["package_name"], "dev.sprite.machine");
        assert_eq!(document[0]["target"]["namespace"], "android_app");
        assert_eq!(
            document[0]["target"]["sha256_cert_fingerprints"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn a_malformed_android_config_serves_nothing() {
        let bad_print = config(&[
            (ANDROID_PACKAGE_NAME, "dev.sprite.machine"),
            (ANDROID_SHA256_CERT_FINGERPRINTS, "AB:CD"),
        ]);
        assert_eq!(asset_links(&bad_print), None);
        let no_dot = config(&[
            (ANDROID_PACKAGE_NAME, "machine"),
            (ANDROID_SHA256_CERT_FINGERPRINTS, &fingerprint()),
        ]);
        assert_eq!(asset_links(&no_dot), None);
    }

    #[test]
    fn deep_links_point_at_the_web_thread_route() {
        assert_eq!(
            deep_link_location("th_01JBX2").as_deref(),
            Some("/#/t/th_01JBX2")
        );
        assert_eq!(deep_link_location("bad id"), None);
        assert_eq!(deep_link_location("a/../b"), None);
    }
}
