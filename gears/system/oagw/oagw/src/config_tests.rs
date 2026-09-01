//! Tests for the OAGW gear configuration.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn defaults_match_specified_values() {
    let cfg = OagwConfig::default();
    assert_eq!(cfg.proxy_timeout_secs, 30);
    assert!(!cfg.allow_http_upstream);
    assert_eq!(cfg.token_cache_ttl_secs, 300);
    assert_eq!(cfg.token_cache_capacity, 10_000);
    assert!(!cfg.ssrf_policy.enabled);
    assert_eq!(cfg.max_payload_bytes, 100 * 1024 * 1024);
    assert_eq!(cfg.plugin_gc_ttl_secs, 30 * 24 * 60 * 60);
    assert_eq!(cfg.proxy_timeout().as_secs(), 30);
    assert_eq!(cfg.token_cache_ttl().as_secs(), 300);
    assert_eq!(cfg.plugin_gc_ttl().as_secs(), 30 * 24 * 60 * 60);
}

#[test]
fn parses_e2e_local_shape() {
    let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false }
    }))
    .unwrap();

    assert_eq!(cfg.proxy_timeout_secs, 2);
    assert!(cfg.allow_http_upstream);
    assert!(!cfg.ssrf_policy.enabled);
    // Untouched fields keep their defaults.
    assert_eq!(cfg.token_cache_capacity, 10_000);
    assert!(cfg.ssrf_policy.deny_private_networks);
}

#[test]
fn parses_empty_and_unknown_payloads_leniently() {
    let empty: OagwConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(empty, OagwConfig::default());

    // The platform forwards unknown keys into the gear block; they must not
    // break deserialisation.
    let extra: OagwConfig = serde_json::from_value(serde_json::json!({
        "some_future_key": 1,
        "proxy_timeout_secs": 5
    }))
    .unwrap();
    assert_eq!(extra.proxy_timeout_secs, 5);
}

#[test]
fn ssrf_policy_defaults_to_disabled() {
    let policy: SsrfPolicyConfig = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(!policy.enabled);
    assert!(policy.allowed_segments.is_empty());
    assert!(policy.deny_private_networks);
}
