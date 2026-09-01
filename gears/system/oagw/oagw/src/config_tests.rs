//! Tests for [`crate::config`].

use crate::config::{OagwConfig, SsrfPolicy};

fn e2e_config() -> OagwConfig {
    OagwConfig {
        proxy_timeout_secs: 2,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: false,
            allow_private_networks: true,
            allowed_ip_ranges: Vec::new(),
        },
        ..OagwConfig::default()
    }
}

#[test]
fn defaults_are_valid_and_documented() {
    let config = OagwConfig::default();
    config.validate().expect("defaults must validate");

    assert_eq!(config.proxy_timeout_secs, 30);
    assert!(!config.allow_http_upstream);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
    assert_eq!(config.upstream_l1_cache_max_entries, 10_000);
    assert_eq!(config.route_l1_cache_max_entries, 10_000);
    assert_eq!(config.plugin_l1_cache_max_entries, 10_000);
    assert_eq!(config.dp_cache_max_entries, 1_000);
    assert!(!config.ssrf_policy.enabled);
    assert!(config.ssrf_policy.allow_private_networks);
    assert_eq!(config.proxy_timeout(), std::time::Duration::from_secs(30));
}

#[test]
fn e2e_config_block_round_trips() {
    // Exactly the block from `config/e2e-local.yaml`: three keys, everything
    // else defaulted. `deny_unknown_fields` must not reject it.
    let raw = serde_json::json!({
        "proxy_timeout_secs": 2,
        "allow_http_upstream": true,
        "ssrf_policy": { "enabled": false }
    });
    let parsed: OagwConfig = serde_json::from_value(raw).expect("e2e block must parse");
    assert_eq!(parsed.proxy_timeout_secs, 2);
    assert!(parsed.allow_http_upstream);
    assert!(!parsed.ssrf_policy.enabled);
    assert_eq!(parsed.token_cache_ttl_secs, 300);
    parsed.validate().expect("e2e block must validate");

    // And the inverse: serialising the e2e config must produce those keys.
    let encoded = serde_json::to_value(e2e_config()).expect("config must serialise");
    assert_eq!(encoded["proxy_timeout_secs"], serde_json::json!(2));
    assert_eq!(encoded["allow_http_upstream"], serde_json::json!(true));
    assert_eq!(encoded["ssrf_policy"]["enabled"], serde_json::json!(false));
}

#[test]
fn unknown_keys_are_rejected() {
    let raw = serde_json::json!({ "proxy_timeout": 5 });
    let error = serde_json::from_value::<OagwConfig>(raw).expect_err("typo must be rejected");
    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[test]
fn ssrf_policy_rejects_unknown_keys_and_empty_ranges() {
    let raw = serde_json::json!({ "enabled": true, "allow_list": ["10.0.0.0/8"] });
    let error = serde_json::from_value::<SsrfPolicy>(raw).expect_err("typo must be rejected");
    assert!(error.to_string().contains("unknown field"), "{error}");

    let config = OagwConfig {
        ssrf_policy: SsrfPolicy {
            enabled: true,
            allow_private_networks: true,
            allowed_ip_ranges: vec!["  ".to_owned()],
        },
        ..OagwConfig::default()
    };
    let error = config.validate().expect_err("empty CIDR must be rejected");
    assert!(error.to_string().contains("allowed_ip_ranges"), "{error}");
}

#[test]
fn every_zero_knob_is_named_by_validate() {
    let config = OagwConfig {
        proxy_timeout_secs: 0,
        token_cache_ttl_secs: 0,
        token_cache_capacity: 0,
        upstream_l1_cache_max_entries: 0,
        route_l1_cache_max_entries: 0,
        plugin_l1_cache_max_entries: 0,
        dp_cache_max_entries: 0,
        ..OagwConfig::default()
    };
    let error = config.validate().expect_err("zero knobs must be rejected");
    let message = error.to_string();
    for name in [
        "proxy_timeout_secs",
        "token_cache_ttl_secs",
        "token_cache_capacity",
        "upstream_l1_cache_max_entries",
        "route_l1_cache_max_entries",
        "plugin_l1_cache_max_entries",
        "dp_cache_max_entries",
    ] {
        assert!(message.contains(name), "{name} missing from: {message}");
    }
}

#[test]
fn json_round_trip_is_lossless() {
    let config = OagwConfig {
        proxy_timeout_secs: 7,
        allow_http_upstream: true,
        ssrf_policy: SsrfPolicy {
            enabled: true,
            allow_private_networks: false,
            allowed_ip_ranges: vec!["10.0.0.0/8".to_owned()],
        },
        token_cache_ttl_secs: 60,
        token_cache_capacity: 16,
        upstream_l1_cache_max_entries: 32,
        route_l1_cache_max_entries: 64,
        plugin_l1_cache_max_entries: 128,
        dp_cache_max_entries: 256,
    };
    let encoded = serde_json::to_string(&config).expect("serialise");
    let decoded: OagwConfig = serde_json::from_str(&encoded).expect("deserialise");
    assert_eq!(decoded, config);
    config.validate().expect("round trip must stay valid");
}

#[test]
fn ssrf_defaults_apply_when_section_is_absent() {
    let parsed: OagwConfig = serde_json::from_value(serde_json::json!({})).expect("empty config");
    assert_eq!(parsed.ssrf_policy, SsrfPolicy::default());
    assert_eq!(parsed.ssrf_policy.allowed_ip_ranges, Vec::<String>::new());
}
