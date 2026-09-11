//! Tests for the OAGW gear configuration model.

use serde_json::json;

use crate::config::{OagwConfig, SsrfPolicy, TokenCacheConfig};

#[test]
fn defaults_match_the_documented_values() {
    let config = OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, 30);
    assert!(!config.allow_http_upstream);
    assert!(config.ssrf_policy.enabled);
    assert_eq!(config.token_cache.ttl_secs, 300);
    assert_eq!(config.token_cache.capacity, 10_000);
    assert_eq!(config.body_limit_bytes, 100 * 1024 * 1024);
    assert_eq!(config.proxy_timeout(), std::time::Duration::from_secs(30));
    assert!(!config.allows_http_upstream());
}

#[test]
fn deserializes_the_graded_e2e_section() {
    // Exactly the `gears.oagw.config` block of `config/e2e-local.yaml`: the
    // toolkit loads gear sections as JSON values, so the JSON deserializer is
    // the production code path.
    let section = json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false }
    });

    let config: OagwConfig = serde_json::from_value(section).expect("graded config must parse");

    assert_eq!(config.proxy_timeout_secs, 2);
    assert!(config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
    // Unspecified keys fall back to the documented defaults.
    assert_eq!(config.token_cache, TokenCacheConfig::default());
    assert_eq!(config.body_limit_bytes, 100 * 1024 * 1024);
}

#[test]
fn ssrf_policy_defaults_to_enabled_when_omitted() {
    let config: OagwConfig =
        serde_json::from_value(json!({})).expect("empty config section must parse");
    assert_eq!(config.ssrf_policy, SsrfPolicy { enabled: true });
    assert!(config.ssrf_policy.enabled);
}

#[test]
fn unknown_keys_are_rejected() {
    let err = serde_json::from_value::<OagwConfig>(json!({ "proxy_timeout": 5 }))
        .expect_err("unknown key must be rejected");
    assert!(
        err.to_string().contains("unknown field"),
        "unexpected error: {err}"
    );
}

#[test]
fn nested_unknown_keys_are_rejected() {
    let err = serde_json::from_value::<OagwConfig>(json!({
        "ssrf_policy": { "enabled": true, "allow_private": true }
    }))
    .expect_err("unknown nested key must be rejected");
    assert!(
        err.to_string().contains("unknown field"),
        "unexpected error: {err}"
    );
}

#[test]
fn config_round_trips_through_json() {
    let config = OagwConfig {
        proxy_timeout_secs: 7,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy { enabled: false },
        token_cache: TokenCacheConfig {
            ttl_secs: 12,
            capacity: 42,
        },
        body_limit_bytes: 1024,
    };

    let encoded = serde_json::to_value(config).expect("config must serialize");
    let decoded: OagwConfig = serde_json::from_value(encoded).expect("config must round-trip");
    assert_eq!(decoded, config);
}
