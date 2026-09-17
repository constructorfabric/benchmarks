use super::*;

#[test]
fn defaults_are_contract_defaults() {
    let config = OagwConfig::default();
    assert_eq!(config.proxy_timeout_secs, 30);
    assert!(!config.allow_http_upstream);
    assert!(config.ssrf_policy.enabled);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
}

#[test]
fn deserializes_the_e2e_config_block() {
    let raw = serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false }
    });
    let config: OagwConfig = serde_json::from_value(raw).expect("valid config");
    assert_eq!(config.proxy_timeout_secs, 2);
    assert!(config.allow_http_upstream);
    assert!(!config.ssrf_policy.enabled);
    assert_eq!(config.token_cache_ttl_secs, 300);
}

#[test]
fn deserializes_from_an_empty_object() {
    let config: OagwConfig = serde_json::from_value(serde_json::json!({})).expect("empty config");
    assert_eq!(config, OagwConfig::default());
}

#[test]
fn ssrf_policy_defaults_to_enabled() {
    let config: OagwConfig = serde_json::from_value(serde_json::json!({ "ssrf_policy": {} }))
        .expect("empty ssrf policy");
    assert!(config.ssrf_policy.enabled);
}
