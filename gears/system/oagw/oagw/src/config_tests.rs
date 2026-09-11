//! Unit tests for the gear configuration.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn defaults_match_the_documented_posture() {
    let cfg = OagwConfig::default();
    assert_eq!(cfg.proxy_timeout_secs, 30);
    assert!(!cfg.allow_http_upstream);
    assert!(cfg.ssrf_policy.enabled);
    assert_eq!(cfg.body_limit_bytes, 100_000_000);
    assert!(cfg.validate().is_ok());
}

#[test]
fn graded_configuration_parses() {
    let cfg: OagwConfig = serde_json::from_value(serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": {"enabled": false}
    }))
    .unwrap();
    assert_eq!(cfg.proxy_timeout_secs, 2);
    assert!(cfg.permits_plaintext_connection());
    assert!(!cfg.ssrf_policy.enabled);
    assert_eq!(cfg.proxy_timeout(), std::time::Duration::from_secs(2));
}

#[test]
fn plaintext_permission_is_separate_from_scheme_acceptance() {
    let cfg = OagwConfig::default();
    // The flag governs connections only; scheme acceptance is a domain concern
    // and is not consulted here.
    assert!(!cfg.permits_plaintext_connection());
    assert!(
        serde_json::from_value::<crate::domain::model::EndpointScheme>(serde_json::json!("http"))
            .is_ok()
    );
}

#[test]
fn unknown_fields_are_rejected() {
    assert!(serde_json::from_value::<OagwConfig>(serde_json::json!({"nope": 1})).is_err());
}

#[test]
fn zero_values_are_rejected() {
    let cfg: OagwConfig =
        serde_json::from_value(serde_json::json!({"proxy_timeout_secs": 0})).unwrap();
    assert!(cfg.validate().is_err());
    let cfg: OagwConfig =
        serde_json::from_value(serde_json::json!({"body_limit_bytes": 0})).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn body_limit_cannot_exceed_the_hard_cap() {
    let cfg: OagwConfig =
        serde_json::from_value(serde_json::json!({"body_limit_bytes": 200_000_000})).unwrap();
    assert!(cfg.validate().is_err());
}
