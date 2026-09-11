//! Unit tests for [`OagwConfig`] deserialization.
//!
//! The gear's config is provided as YAML by the host, but the shape is identical
//! once deserialized, so JSON fixtures exercise the same `serde` attributes.

use super::*;

#[test]
fn graded_configuration_parses() {
    let json = r#"
{
  "proxy_timeout_secs": 2,
  "allow_http_upstream": true,
  "ssrf_policy": { "enabled": false }
}"#;
    let cfg: OagwConfig = serde_json::from_str(json).expect("graded config parses");
    assert_eq!(cfg.proxy_timeout_secs, 2);
    assert!(cfg.allow_http_upstream);
    assert!(!cfg.ssrf_policy.enabled);
    assert_eq!(cfg.proxy_timeout(), Duration::from_secs(2));
}

#[test]
fn empty_object_yields_defaults() {
    let cfg: OagwConfig = serde_json::from_str("{}").expect("empty config parses");
    assert_eq!(cfg.proxy_timeout_secs, 30);
    assert!(!cfg.allow_http_upstream);
    assert_eq!(cfg.max_body_bytes(), 100 * 1024 * 1024);
    assert_eq!(cfg.token_cache.ttl_secs, 300);
    assert_eq!(cfg.token_cache.capacity, 10_000);
    assert_eq!(cfg.l1_cache_capacity, 1000);
}

#[test]
fn token_cache_settings_are_read() {
    let cfg: OagwConfig =
        serde_json::from_str(r#"{"token_cache": {"ttl_secs": 60, "capacity": 7}}"#).unwrap();
    #[allow(clippy::duration_suboptimal_units)] // seconds are the config unit
    let expected = Duration::from_secs(60);
    assert_eq!(cfg.token_cache.ttl(), expected);
    assert_eq!(cfg.token_cache.capacity, 7);
}

#[test]
fn ssrf_policy_defaults_block_private_networks_when_enabled() {
    let cfg: SsrfPolicy = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
    assert!(cfg.enabled);
    assert!(cfg.block_private_networks);
}
