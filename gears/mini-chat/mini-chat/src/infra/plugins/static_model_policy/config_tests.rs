#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};

use super::StaticModelPolicyConfig;

fn entry(id: &str) -> Value {
    json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "Premium",
        "enabled": true,
        "context_window": 1000,
        "max_output_tokens": 100,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "", "available_from": "", "max_file_size_mb": 1,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": false },
            "tool_support": {
                "web_search": false, "file_search": false, "image_generation": false,
                "code_interpreter": false, "mcp": false
            },
            "supported_endpoints": {}
        }
    })
}

fn parse(config: Value) -> Result<StaticModelPolicyConfig, serde_json::Error> {
    serde_json::from_value(config)
}

fn valid(config: Value) -> bool {
    parse(config).is_ok_and(|cfg| cfg.validate().is_ok())
}

#[test]
fn an_absent_section_uses_the_defaults() {
    let cfg = StaticModelPolicyConfig::default();

    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert!(cfg.model_catalog.is_empty());
    assert_eq!(
        cfg.default_standard_limits.limit_daily_credits_micro,
        100_000_000
    );
    assert_eq!(
        cfg.default_standard_limits.limit_monthly_credits_micro,
        1_000_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_daily_credits_micro,
        50_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_monthly_credits_micro,
        500_000_000
    );
    assert!(cfg.validate().is_ok());
}

#[test]
fn model_catalog_is_required_when_the_section_is_present() {
    assert!(parse(json!({})).is_err());
    assert!(parse(json!({ "vendor": "acme" })).is_err());
    assert!(valid(json!({ "model_catalog": [] })));
}

#[test]
fn unknown_keys_are_rejected_at_the_top_level_and_in_kill_switches() {
    assert!(parse(json!({ "model_catalog": [], "bogus": 1 })).is_err());
    assert!(
        parse(json!({ "model_catalog": [], "kill_switches": { "disable_premium": true } }))
            .is_err()
    );
}

#[test]
fn kill_switches_default_to_false_and_each_may_be_omitted() {
    let cfg = parse(json!({
        "model_catalog": [],
        "kill_switches": { "disable_web_search": true }
    }))
    .unwrap();

    assert!(cfg.kill_switches.disable_web_search);
    assert!(!cfg.kill_switches.disable_premium_tier);
    assert!(!cfg.kill_switches.disable_images);
}

#[test]
fn unknown_keys_inside_catalog_entries_and_limits_are_ignored() {
    let mut model = entry("a");
    model["bogus"] = json!(true);

    assert!(valid(json!({
        "model_catalog": [model],
        "default_standard_limits": {
            "limit_daily_credits_micro": 1, "limit_monthly_credits_micro": 2, "bogus": 3
        }
    })));
}

#[test]
fn credit_multipliers_must_be_in_range() {
    for (field, value, accepted) in [
        ("input_tokens_credit_multiplier_micro", 0_u64, false),
        ("input_tokens_credit_multiplier_micro", 1, true),
        ("input_tokens_credit_multiplier_micro", 10_000_000_000, true),
        (
            "input_tokens_credit_multiplier_micro",
            10_000_000_001,
            false,
        ),
        ("output_tokens_credit_multiplier_micro", 0, false),
        (
            "output_tokens_credit_multiplier_micro",
            10_000_000_001,
            false,
        ),
    ] {
        let mut model = entry("a");
        model[field] = json!(value);
        assert_eq!(
            valid(json!({ "model_catalog": [model] })),
            accepted,
            "{field}={value}"
        );
    }
}

#[test]
fn bytes_per_token_must_be_positive() {
    let mut model = entry("a");
    model["estimation_budgets"] = json!({ "bytes_per_token_conservative": 0 });

    assert!(!valid(json!({ "model_catalog": [model] })));
}

#[test]
fn model_ids_must_be_unique_and_non_empty() {
    assert!(!valid(json!({ "model_catalog": [entry("a"), entry("a")] })));
    assert!(!valid(json!({ "model_catalog": [entry(" ")] })));
    assert!(valid(json!({ "model_catalog": [entry("a"), entry("b")] })));
}

#[test]
fn the_vendor_must_be_non_empty() {
    assert!(!valid(json!({ "model_catalog": [], "vendor": "" })));
}
