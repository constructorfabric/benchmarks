//! Gear configuration defaults and validation.

use super::*;

#[test]
fn the_defaults_are_the_safe_posture() {
    let config = OagwConfig::default();
    assert!(!config.allow_http_upstream, "plaintext upstreams are opt-in");
    assert!(config.ssrf_policy.enabled, "SSRF guardrails are on by default");
    assert!(!config.ssrf_policy.allow_loopback);
    assert!(!config.ssrf_policy.allow_private);
    assert!(!config.ssrf_policy.allow_link_local);
    assert_eq!(config.max_request_body_bytes, BODY_LIMIT_BYTES);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
    assert_eq!(config.default_page_size, 50);
    assert_eq!(config.max_page_size, 100);
}

#[test]
fn the_shipped_e2e_config_shape_deserializes() {
    let config: OagwConfig = serde_json::from_value(serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": {"enabled": false},
    }))
    .unwrap();
    assert_eq!(config.proxy_timeout().as_secs(), 2);
    assert!(config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
    // Unspecified keys keep their defaults.
    assert_eq!(config.max_page_size, 100);
}

#[test]
fn unknown_configuration_keys_do_not_stop_the_gear() {
    let config: OagwConfig =
        serde_json::from_value(serde_json::json!({"from_a_newer_build": 42})).unwrap();
    assert_eq!(config.proxy_timeout_secs, 30);
}

#[test]
fn timeouts_never_collapse_to_zero() {
    let config: OagwConfig = serde_json::from_value(serde_json::json!({
        "proxy_timeout_secs": 0,
        "connect_timeout_secs": 0,
        "token_cache_ttl_secs": 0,
    }))
    .unwrap();
    assert_eq!(config.proxy_timeout().as_secs(), 1);
    assert_eq!(config.connect_timeout().as_secs(), 1);
    assert_eq!(config.token_cache_ttl().as_secs(), 1);
}

#[test]
fn page_sizes_are_clamped_into_range() {
    let config = OagwConfig::default();
    assert_eq!(config.clamp_page_size(None), 50);
    assert_eq!(config.clamp_page_size(Some(10)), 10);
    assert_eq!(config.clamp_page_size(Some(1_000)), 100);
    assert_eq!(config.clamp_page_size(Some(0)), 1);
}

#[test]
fn validation_rejects_an_unusable_body_limit_or_page_size() {
    let mut config = OagwConfig::default();
    config.max_request_body_bytes = 0;
    assert!(config.validate().is_err());

    let mut config = OagwConfig::default();
    config.max_request_body_bytes = BODY_LIMIT_BYTES + 1;
    assert!(config.validate().is_err());

    let mut config = OagwConfig::default();
    config.max_page_size = 0;
    assert!(config.validate().is_err());

    assert!(OagwConfig::default().validate().is_ok());
}
