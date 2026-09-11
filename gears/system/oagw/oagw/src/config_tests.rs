//! Tests for [`OagwConfig`].

use std::time::Duration;

use crate::config::{
    DEFAULT_PROXY_TIMEOUT_SECS, OagwConfig, SsrfPolicy, TokenCacheConfig,
};

#[test]
fn defaults_are_conservative() {
    let config = OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
    // Plaintext is opt-in: the default upstream connection is TLS only.
    assert!(!config.allow_http_upstream);
    assert!(config.ssrf_policy.enabled);
    assert!(config.ssrf_policy.deny_hosts.is_empty());
    assert_eq!(config.token_cache.ttl_secs, 300);
    assert_eq!(config.token_cache.capacity, 1024);
}

#[test]
fn proxy_timeout_never_reaches_zero() {
    let mut config = OagwConfig::default();
    config.proxy_timeout_secs = 0;
    assert_eq!(config.proxy_timeout(), Duration::from_secs(1));
    config.proxy_timeout_secs = 5;
    assert_eq!(config.proxy_timeout(), Duration::from_secs(5));
}

#[test]
fn plaintext_permitted_only_when_enabled() {
    let mut config = OagwConfig::default();
    assert!(!config.permits_plaintext("http"));
    assert!(!config.permits_plaintext("ws"));
    assert!(config.permits_plaintext("https"));
    assert!(config.permits_plaintext("wss"));
    assert!(config.permits_plaintext("wt"));

    config.allow_http_upstream = true;
    assert!(config.permits_plaintext("http"));
    assert!(config.permits_plaintext("ws"));
    // Turning plaintext on does not turn the secure schemes off.
    assert!(config.permits_plaintext("https"));
}

#[test]
fn an_absent_configuration_block_parses_to_the_defaults() {
    let parsed: OagwConfig = serde_json::from_str("{}").expect("an empty object is valid");
    assert_eq!(parsed, OagwConfig::default());
}

#[test]
fn partial_configuration_inherits_its_defaults() {
    let parsed: OagwConfig =
        serde_json::from_str(r#"{ "allow_http_upstream": true }"#).expect("parses");
    assert!(parsed.allow_http_upstream);
    assert_eq!(parsed.proxy_timeout_secs, DEFAULT_PROXY_TIMEOUT_SECS);
    assert_eq!(parsed.token_cache.ttl_secs, 300);
}

#[test]
fn unknown_configuration_keys_are_rejected() {
    let err = serde_json::from_str::<OagwConfig>(r#"{ "proxytimeouts": 1 }"#)
        .err()
        .expect("an unknown key is not accepted");
    assert!(err.to_string().contains("unknown field"));
}

#[test]
fn ssrf_policy_round_trips_its_host_lists() {
    let json = r#"{ "enabled": false, "deny_hosts": ["metadata.internal"], "allow_hosts": ["loopback.test"] }"#;
    let parsed: SsrfPolicy = serde_json::from_str(json).expect("parses");
    assert!(!parsed.enabled);
    assert_eq!(parsed.deny_hosts, vec!["metadata.internal".to_owned()]);
    assert_eq!(parsed.allow_hosts, vec!["loopback.test".to_owned()]);
}

#[test]
fn token_cache_settings_round_trip() {
    let parsed: TokenCacheConfig =
        serde_json::from_str(r#"{ "ttl_secs": 30, "capacity": 8 }"#).expect("parses");
    assert_eq!(parsed.ttl_secs, 30);
    assert_eq!(parsed.capacity, 8);
}

#[test]
fn the_configuration_serializes_back_to_a_stable_shape() {
    let config = OagwConfig::default();
    let value = serde_json::to_value(&config).expect("serializes");
    let object = value.as_object().expect("an object");
    for key in [
        "proxy_timeout_secs",
        "allow_http_upstream",
        "ssrf_policy",
        "token_cache",
    ] {
        assert!(object.contains_key(key), "missing key {key}");
    }
}
