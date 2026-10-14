//! Quota configuration tests: the values the quota rules read from the validated gear
//! configuration, their documented defaults and the floor taken from the gear configuration only.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration`,
//! definitions of done `cpt-cf-mini-chat-dod-quota-and-billing-warning-thresholds` and
//! `cpt-cf-mini-chat-dod-quota-and-billing-deferred-scope`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};

use super::QuotaSettings;
use crate::config::MiniChatConfig;
use crate::test_support::minimal_gear_config;

fn config_with(extra: &Value) -> MiniChatConfig {
    let mut raw = minimal_gear_config();
    for (key, value) in extra.as_object().unwrap() {
        raw[key] = value.clone();
    }
    MiniChatConfig::from_value(raw).expect("valid config")
}

#[test]
fn the_defaults_are_the_documented_values() {
    let settings = QuotaSettings::from_config(&config_with(&json!({})));
    assert!((settings.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(settings.warning_threshold_pct, 80);
    assert_eq!(settings.web_search_daily_quota, 75);
    assert_eq!(settings.code_interpreter_daily_quota, 50);
    assert_eq!(settings.streaming_max_output_tokens, 32_768);
    assert_eq!(settings.minimal_generation_floor, 50);
}

#[test]
fn configured_values_are_read() {
    let settings = QuotaSettings::from_config(&config_with(&json!({
        "quota": {
            "overshoot_tolerance_factor": 1.25,
            "warning_threshold_pct": 90,
            "web_search_daily_quota": 5,
            "code_interpreter_daily_quota": 7
        },
        "streaming": { "max_output_tokens": 4096 },
        "estimation_budgets": { "minimal_generation_floor": 100 }
    })));
    assert!((settings.overshoot_tolerance_factor - 1.25).abs() < f64::EPSILON);
    assert_eq!(settings.warning_threshold_pct, 90);
    assert_eq!(settings.web_search_daily_quota, 5);
    assert_eq!(settings.code_interpreter_daily_quota, 7);
    assert_eq!(settings.streaming_max_output_tokens, 4096);
    assert_eq!(settings.minimal_generation_floor, 100);
}

#[test]
fn the_deprecated_budget_keys_never_change_the_floor_or_any_other_value() {
    let plain = QuotaSettings::from_config(&config_with(&json!({})));
    let with_deprecated = QuotaSettings::from_config(&config_with(&json!({
        "estimation_budgets": {
            "bytes_per_token_conservative": 9,
            "fixed_overhead_tokens": 999,
            "safety_margin_pct": 99,
            "image_token_budget": 9,
            "tool_surcharge_tokens": 9,
            "web_search_surcharge_tokens": 9,
            "code_interpreter_surcharge_tokens": 9
        }
    })));
    assert_eq!(plain, with_deprecated);
}

#[test]
fn the_per_message_maxima_are_not_part_of_the_quota_rules() {
    // they are enforced mid-turn by the streaming feature: changing them changes nothing here
    let plain = QuotaSettings::from_config(&config_with(&json!({})));
    let changed = QuotaSettings::from_config(&config_with(&json!({
        "quota": {
            "web_search_max_calls_per_message": 9,
            "code_interpreter_max_calls_per_message": 20
        }
    })));
    assert_eq!(plain, changed);
}
