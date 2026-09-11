//! Configuration tests (T003, T012): defaults and the HTTPS-only posture.

use crate::config::{OagwConfig, SsrfPolicy, TokenCacheConfig};

#[test]
fn plaintext_upstreams_are_disabled_by_default() {
    let config = OagwConfig::default();
    assert!(
        !config.allow_http_upstream,
        "the gear must default to the HTTPS-only posture"
    );
}

#[test]
fn the_proxy_timeout_defaults_to_thirty_seconds() {
    let config = OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, 30);
}

#[test]
fn the_token_cache_defaults_are_documented_ones() {
    let config = TokenCacheConfig::default();
    assert_eq!(config.ttl_secs, 300);
    assert_eq!(config.capacity, 10_000);
}

#[test]
fn ssrf_protection_is_enabled_by_default() {
    let policy = SsrfPolicy::default();
    assert!(policy.enabled);
}

#[test]
fn an_empty_config_node_deserialises() {
    let config: OagwConfig =
        serde_json::from_value(serde_json::json!({})).expect("defaults apply");
    assert!(!config.allow_http_upstream);
    assert_eq!(config.proxy_timeout_secs, 30);
}

#[test]
fn the_e2e_posture_enables_plaintext_upstreams() {
    // `allow_http_upstream` governs whether a plaintext connection is made,
    // never whether `http` is an accepted scheme value.
    let config: OagwConfig = serde_json::from_value(serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false }
    }))
    .expect("the e2e node parses");
    assert!(config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
    assert_eq!(config.proxy_timeout_secs, 2);
}
