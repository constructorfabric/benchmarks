//! Plugin config tests: defaults, required `model_catalog`, unknown keys, nested-type leniency
//! and the credit multiplier range (DESIGN Appendix B.1, algorithm `static-policy-plugin`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{ModelTier, TierLimits};
use serde_json::{Value, json};

use super::{StaticKillSwitchesConfig, StaticModelPolicyConfig};
use crate::test_support::entry_json;

fn parse(raw: Value) -> StaticModelPolicyConfig {
    StaticModelPolicyConfig::parse(raw).expect("config must parse")
}

fn load(raw: &Value) -> Result<StaticModelPolicyConfig, String> {
    let cfg = StaticModelPolicyConfig::parse(raw.clone()).map_err(|e| e.to_string())?;
    cfg.validate().map_err(|e| e.to_string())?;
    Ok(cfg)
}

fn catalog_with(patch: &Value) -> Value {
    let mut entry = entry_json("gpt-a", "standard", true);
    for (key, value) in patch.as_object().unwrap() {
        entry[key] = value.clone();
    }
    json!({ "model_catalog": [entry] })
}

#[test]
fn an_absent_section_uses_the_documented_defaults() {
    let cfg = StaticModelPolicyConfig::default();
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert!(cfg.model_catalog.is_empty());
    assert_eq!(cfg.kill_switches, StaticKillSwitchesConfig::default());
    assert_eq!(
        cfg.default_standard_limits,
        TierLimits {
            limit_daily_credits_micro: 100_000_000,
            limit_monthly_credits_micro: 1_000_000_000
        }
    );
    assert_eq!(
        cfg.default_premium_limits,
        TierLimits {
            limit_daily_credits_micro: 50_000_000,
            limit_monthly_credits_micro: 500_000_000
        }
    );
    cfg.validate().expect("defaults are valid");
}

#[test]
fn a_present_section_applies_defaults_to_the_other_keys() {
    let cfg = parse(json!({ "model_catalog": [] }));
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert_eq!(
        cfg.default_standard_limits.limit_daily_credits_micro,
        100_000_000
    );
    assert_eq!(
        cfg.default_premium_limits.limit_monthly_credits_micro,
        500_000_000
    );
}

#[test]
fn model_catalog_is_a_required_key_when_the_section_is_present() {
    for raw in [
        json!({}),
        json!({ "vendor": "acme" }),
        json!({ "priority": 5 }),
    ] {
        let message = StaticModelPolicyConfig::parse(raw)
            .expect_err("must be rejected")
            .to_string();
        assert!(message.contains("model_catalog"), "{message}");
    }
    assert!(
        parse(json!({ "model_catalog": [] }))
            .model_catalog
            .is_empty()
    );
}

#[test]
fn unknown_top_level_keys_are_rejected() {
    let message = StaticModelPolicyConfig::parse(json!({ "model_catalog": [], "bogus_key": 1 }))
        .expect_err("unknown key")
        .to_string();
    assert!(message.contains("bogus_key"), "{message}");
}

#[test]
fn vendor_priority_and_limits_are_read() {
    let cfg = parse(json!({
        "vendor": "acme",
        "priority": 7,
        "model_catalog": [],
        "default_standard_limits": { "limit_daily_credits_micro": 1, "limit_monthly_credits_micro": 2 },
        "default_premium_limits": { "limit_daily_credits_micro": 3, "limit_monthly_credits_micro": 4 }
    }));
    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.priority, 7);
    assert_eq!(
        cfg.default_standard_limits,
        TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 2
        }
    );
    assert_eq!(
        cfg.default_premium_limits,
        TierLimits {
            limit_daily_credits_micro: 3,
            limit_monthly_credits_micro: 4
        }
    );
}

#[test]
fn priority_is_an_i16() {
    assert!(
        StaticModelPolicyConfig::parse(json!({ "priority": 40_000, "model_catalog": [] })).is_err()
    );
    assert_eq!(
        parse(json!({ "priority": -5, "model_catalog": [] })).priority,
        -5
    );
}

// ── kill switches ────────────────────────────────────────────────────────────

#[test]
fn kill_switches_may_be_omitted_wholly_or_per_field_and_read_as_false() {
    let none = parse(json!({ "model_catalog": [] }));
    assert_eq!(none.kill_switches, StaticKillSwitchesConfig::default());

    let partial =
        parse(json!({ "model_catalog": [], "kill_switches": { "disable_web_search": true } }));
    assert!(partial.kill_switches.disable_web_search);
    assert!(!partial.kill_switches.disable_premium_tier);
    assert!(!partial.kill_switches.force_standard_tier);
    assert!(!partial.kill_switches.disable_file_search);
    assert!(!partial.kill_switches.disable_images);
    assert!(!partial.kill_switches.disable_code_interpreter);

    let empty = parse(json!({ "model_catalog": [], "kill_switches": {} }));
    assert_eq!(empty.kill_switches, StaticKillSwitchesConfig::default());
}

#[test]
fn each_of_the_six_kill_switches_is_accepted() {
    for key in [
        "disable_premium_tier",
        "force_standard_tier",
        "disable_web_search",
        "disable_file_search",
        "disable_images",
        "disable_code_interpreter",
    ] {
        let cfg = parse(json!({ "model_catalog": [], "kill_switches": { key: true } }));
        let set = [
            cfg.kill_switches.disable_premium_tier,
            cfg.kill_switches.force_standard_tier,
            cfg.kill_switches.disable_web_search,
            cfg.kill_switches.disable_file_search,
            cfg.kill_switches.disable_images,
            cfg.kill_switches.disable_code_interpreter,
        ];
        assert_eq!(set.iter().filter(|b| **b).count(), 1, "{key}");
    }
}

#[test]
fn an_unknown_kill_switch_is_a_config_error() {
    let message = StaticModelPolicyConfig::parse(json!({
        "model_catalog": [],
        "kill_switches": { "disable_web_serach": true }
    }))
    .expect_err("misspelled switch")
    .to_string();
    assert!(message.contains("disable_web_serach"), "{message}");
}

// ── nested SDK types ─────────────────────────────────────────────────────────

#[test]
fn unknown_keys_inside_the_nested_sdk_types_are_ignored() {
    let mut raw = catalog_with(&json!({
        "future_entry_key": 1,
        "estimation_budgets": { "safety_margin_pct": 20, "future_budget": 1 }
    }));
    raw["model_catalog"][0]["general_config"]["future_general_key"] = json!(true);
    raw["default_standard_limits"] = json!({
        "limit_daily_credits_micro": 1,
        "limit_monthly_credits_micro": 2,
        "limit_4h_credits_micro": 3
    });
    raw["default_premium_limits"] = json!({
        "limit_daily_credits_micro": 1,
        "limit_monthly_credits_micro": 2,
        "future": 0
    });
    let cfg = parse(raw);
    assert_eq!(
        cfg.model_catalog[0].estimation_budgets.safety_margin_pct,
        20
    );
}

#[test]
fn a_catalog_entry_missing_a_required_key_fails() {
    let mut raw = catalog_with(&json!({}));
    raw["model_catalog"][0]
        .as_object_mut()
        .unwrap()
        .remove("id");
    assert!(StaticModelPolicyConfig::parse(raw).is_err());
}

#[test]
fn the_capitalised_tier_spelling_of_the_sample_config_is_accepted() {
    let cfg = parse(catalog_with(&json!({ "tier": "Premium" })));
    assert_eq!(cfg.model_catalog[0].tier, ModelTier::Premium);
    let cfg = parse(catalog_with(&json!({ "tier": "Standard" })));
    assert_eq!(cfg.model_catalog[0].tier, ModelTier::Standard);
}

// ── credit multiplier range ──────────────────────────────────────────────────

#[test]
fn credit_multipliers_must_be_within_one_and_ten_billion() {
    for key in [
        "input_tokens_credit_multiplier_micro",
        "output_tokens_credit_multiplier_micro",
    ] {
        for good in [1u64, 2_500_000, 10_000_000_000] {
            load(&catalog_with(&json!({ key: good })))
                .unwrap_or_else(|e| panic!("{key}={good}: {e}"));
        }
        for bad in [0u64, 10_000_000_001, u64::MAX] {
            let message = load(&catalog_with(&json!({ key: bad }))).expect_err("out of range");
            assert!(message.contains(key), "{key}={bad}: {message}");
            assert!(
                message.contains("gpt-a"),
                "the entry id must be named: {message}"
            );
        }
    }
}

#[test]
fn the_multiplier_check_covers_every_entry_including_disabled_ones() {
    let mut raw = catalog_with(&json!({}));
    let mut bad = entry_json("gpt-bad", "premium", false);
    bad["output_tokens_credit_multiplier_micro"] = json!(0);
    raw["model_catalog"].as_array_mut().unwrap().push(bad);
    let message = load(&raw).expect_err("second entry is out of range");
    assert!(message.contains("gpt-bad"), "{message}");
}

#[test]
fn an_empty_catalog_is_valid() {
    load(&json!({ "model_catalog": [] })).expect("empty catalog is valid");
}
