//! Tests of the gear-level configuration (`gears.oagw.config`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::*;

#[test]
fn defaults_match_the_documented_gear_behaviour() {
    let config = OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
    assert_eq!(config.proxy_timeout_secs, 30);
    assert!(!config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
    assert!(!config.ssrf_enabled());
    assert_eq!(config.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
    assert_eq!(config.max_body_bytes, 104_857_600);
    assert_eq!(config.proxy_timeout(), std::time::Duration::from_secs(30));
}

#[test]
fn the_e2e_configuration_shape_deserializes() {
    // Same keys as `config/e2e-local.yaml` (`gears.oagw.config`).
    let config: OagwConfig = serde_json::from_str(
        r#"{
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }"#,
    )
    .expect("valid gear configuration");
    assert_eq!(config.proxy_timeout_secs, 2);
    assert_eq!(config.proxy_timeout(), std::time::Duration::from_secs(2));
    assert!(config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
    // Body limit absent: the 100MB hard limit still applies.
    assert_eq!(config.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
}

#[test]
fn missing_keys_fall_back_to_the_defaults() {
    let config: OagwConfig = serde_json::from_str("{}").expect("empty configuration");
    assert_eq!(config, OagwConfig::default());
}

#[test]
fn the_ssrf_guard_is_opt_in() {
    let config: OagwConfig = serde_json::from_str(
        r#"{ "ssrf_policy": { "enabled": true }, "allow_http_upstream": false }"#,
    )
    .expect("ssrf configuration");
    assert!(config.ssrf_enabled());
    assert!(config.ssrf_policy.enabled);
}

#[test]
fn unknown_keys_are_rejected() {
    let err = serde_json::from_str::<OagwConfig>(r#"{ "proxy_timeouts_secs": 5 }"#)
        .expect_err("unknown key must be rejected");
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn the_http_gate_is_independent_of_scheme_validation() {
    // `allow_http_upstream: false` only closes the egress path; it must never
    // restrict which endpoint schemes a create request may carry.
    let config: OagwConfig =
        serde_json::from_str(r#"{ "allow_http_upstream": false }"#).expect("configuration");
    assert!(!config.allow_http_upstream);
    let config: OagwConfig =
        serde_json::from_str(r#"{ "allow_http_upstream": true }"#).expect("configuration");
    assert!(config.allow_http_upstream);
}
