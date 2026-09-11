//! Unit tests for the gear configuration.

use super::*;

#[test]
fn defaults_are_sane() {
    let config = OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, 30);
    assert!(!config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
    assert!(config.validate().is_ok());
}

#[test]
fn zero_timeout_is_rejected() {
    let config = OagwConfig {
        proxy_timeout_secs: 0,
        ..OagwConfig::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn absurd_timeout_is_rejected() {
    let config = OagwConfig {
        proxy_timeout_secs: 10_000,
        ..OagwConfig::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn zero_cache_settings_are_rejected() {
    assert!(OagwConfig {
        token_cache_ttl_secs: 0,
        ..OagwConfig::default()
    }
    .validate()
    .is_err());
    assert!(OagwConfig {
        token_cache_capacity: 0,
        ..OagwConfig::default()
    }
    .validate()
    .is_err());
}

#[test]
fn unknown_fields_are_rejected() {
    let raw = r#"{ "proxy_timeout_secs": 2, "allow_http_upstream": true,
                   "ssrf_policy": { "enabled": false }, "unknown_key": 1 }"#;
    let parsed: Result<OagwConfig, _> = serde_json::from_str(raw);
    assert!(parsed.is_err());
}

#[test]
fn the_e2e_configuration_parses() {
    let raw = r#"{ "proxy_timeout_secs": 2, "allow_http_upstream": true,
                   "ssrf_policy": { "enabled": false } }"#;
    let parsed: OagwConfig = serde_json::from_str(raw).expect("valid config");
    assert_eq!(parsed.proxy_timeout_secs, 2);
    assert!(parsed.allow_http_upstream);
    assert_eq!(parsed.token_cache_ttl_secs, 300);
}
