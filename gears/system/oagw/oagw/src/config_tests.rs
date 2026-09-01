#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, OagwConfig};

#[test]
fn default_config_is_valid() {
    let cfg = OagwConfig::default();
    assert_eq!(cfg.proxy_timeout_secs, 2);
    assert!(!cfg.allow_http_upstream, "HTTPS-only by default");
    assert!(cfg.ssrf_policy.enabled);
    assert_eq!(cfg.list_default_page_size, DEFAULT_PAGE_SIZE);
    assert_eq!(cfg.list_max_page_size, MAX_PAGE_SIZE);
    assert!(cfg.validate().is_ok());
}

#[test]
fn deserializes_partial_config_with_defaults() {
    let cfg: OagwConfig = serde_json::from_str(r#"{"proxy_timeout_secs": 5}"#)
        .expect("partial config must deserialize");
    assert_eq!(cfg.proxy_timeout_secs, 5);
    assert!(!cfg.allow_http_upstream);
    assert!(cfg.ssrf_policy.enabled);
    assert_eq!(cfg.list_default_page_size, 50);
}

#[test]
fn deserializes_the_shipped_manifest_shape() {
    // `config/e2e-local.yaml` ships this exact block; it must keep binding.
    let cfg: OagwConfig = serde_json::from_str(
        r#"{
            "proxy_timeout_secs": 2,
            "allow_http_upstream": true,
            "ssrf_policy": { "enabled": false }
        }"#,
    )
    .expect("manifest config must deserialize");
    assert!(cfg.allow_http_upstream);
    assert!(!cfg.ssrf_policy.enabled);
    assert!(cfg.validate().is_ok());
}

#[test]
fn rejects_unknown_config_keys() {
    let err = serde_json::from_str::<OagwConfig>(r#"{"proxy_timeouts_secs": 2}"#)
        .expect_err("unknown key must be refused");
    assert!(err.to_string().contains("unknown field"), "{err}");
}

#[test]
fn validate_rejects_each_invalid_field() {
    let zero_timeout = OagwConfig {
        proxy_timeout_secs: 0,
        ..OagwConfig::default()
    };
    assert!(zero_timeout.validate().is_err());

    let zero_page = OagwConfig {
        list_default_page_size: 0,
        ..OagwConfig::default()
    };
    assert!(zero_page.validate().is_err());

    let inverted = OagwConfig {
        list_default_page_size: 200,
        list_max_page_size: 100,
        ..OagwConfig::default()
    };
    assert!(inverted.validate().is_err());
}
