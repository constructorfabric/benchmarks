#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the configuration part of
//! `cpt-cf-mini-chat-algo-gear-foundation-static-policy-plugin`
//! (steps `inst-parse-plugin-config`, `inst-plugin-defaults`, `inst-if-bad-entry`).

use mini_chat_sdk::{KillSwitches, TierLimits};
use serde_json::{Value, json};

use super::{StaticModelPolicyConfig, StaticModelPolicyConfigError};

fn general_config() -> Value {
    json!({
        "type": "model.general.v1",
        "available_from": "2026-01-01T00:00:00Z",
        "max_file_size_mb": 25,
        "api_params": { "stop": [] },
        "features": { "streaming": true, "structured_output": false },
        "tool_support": {
            "web_search": true, "file_search": true, "image_generation": false,
            "code_interpreter": true, "mcp": false
        },
        "supported_endpoints": {
            "chat_completions": false, "responses": true, "embeddings": false,
            "image_generation": false, "audio_speech_generation": false,
            "audio_transcription": false, "audio_translation": false
        }
    })
}

fn entry(model_id: &str) -> Value {
    json!({
        "id": model_id,
        "provider_model_id": model_id,
        "display_name": "Model",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "premium",
        "enabled": true,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 2_500_000,
        "output_tokens_credit_multiplier_micro": 2_500_000,
        "max_num_results": 5,
        "general_config": general_config(),
    })
}

fn with(mut entry: Value, key: &str, value: Value) -> Value {
    entry[key] = value;
    entry
}

fn section(catalog: &[Value]) -> Value {
    json!({ "model_catalog": catalog })
}

fn load(raw: &Value) -> Result<StaticModelPolicyConfig, StaticModelPolicyConfigError> {
    StaticModelPolicyConfig::load(Some(raw))
}

fn load_ok(raw: &Value) -> StaticModelPolicyConfig {
    load(raw).unwrap_or_else(|e| panic!("expected a valid section, got: {e}"))
}

fn load_err(raw: &Value) -> StaticModelPolicyConfigError {
    load(raw).expect_err("expected a configuration error")
}

fn assert_defaults(cfg: &StaticModelPolicyConfig) {
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.priority, 100);
    assert_eq!(
        cfg.kill_switches,
        KillSwitches {
            disable_premium_tier: false,
            force_standard_tier: false,
            disable_web_search: false,
            disable_file_search: false,
            disable_images: false,
            disable_code_interpreter: false,
        }
    );
    assert_eq!(
        cfg.default_standard_limits,
        TierLimits {
            limit_daily_credits_micro: 100_000_000,
            limit_monthly_credits_micro: 1_000_000_000,
        }
    );
    assert_eq!(
        cfg.default_premium_limits,
        TierLimits {
            limit_daily_credits_micro: 50_000_000,
            limit_monthly_credits_micro: 500_000_000,
        }
    );
}

// ---- defaults -----------------------------------------------------------------------

#[test]
fn absent_section_uses_all_defaults_with_an_empty_catalog() {
    let cfg = StaticModelPolicyConfig::load(None).expect("absent section is valid");

    assert_defaults(&cfg);
    assert!(cfg.model_catalog.is_empty());
}

#[test]
fn section_with_only_an_empty_catalog_uses_the_defaults() {
    let cfg = load_ok(&section(&[]));

    assert_defaults(&cfg);
    assert!(cfg.model_catalog.is_empty());
}

#[test]
fn explicit_values_override_the_defaults() {
    let cfg = load_ok(&json!({
        "vendor": "acme",
        "priority": -3,
        "model_catalog": [],
        "kill_switches": { "disable_images": true },
        "default_standard_limits": {
            "limit_daily_credits_micro": 7,
            "limit_monthly_credits_micro": 70
        },
        "default_premium_limits": {
            "limit_daily_credits_micro": 3,
            "limit_monthly_credits_micro": 30
        }
    }));

    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.priority, -3);
    assert!(cfg.kill_switches.disable_images);
    assert_eq!(cfg.default_standard_limits.limit_daily_credits_micro, 7);
    assert_eq!(cfg.default_standard_limits.limit_monthly_credits_micro, 70);
    assert_eq!(cfg.default_premium_limits.limit_daily_credits_micro, 3);
    assert_eq!(cfg.default_premium_limits.limit_monthly_credits_micro, 30);
}

#[test]
fn omitted_kill_switch_fields_default_to_false() {
    let cfg = load_ok(&json!({
        "model_catalog": [],
        "kill_switches": { "disable_web_search": true }
    }));

    assert!(cfg.kill_switches.disable_web_search);
    assert!(!cfg.kill_switches.disable_premium_tier);
    assert!(!cfg.kill_switches.force_standard_tier);
    assert!(!cfg.kill_switches.disable_file_search);
    assert!(!cfg.kill_switches.disable_images);
    assert!(!cfg.kill_switches.disable_code_interpreter);
}

#[test]
fn catalog_entries_are_loaded_in_order() {
    let cfg = load_ok(&section(&[entry("gpt-5.2"), entry("gpt-4.1-mini")]));

    let ids: Vec<&str> = cfg.model_catalog.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["gpt-5.2", "gpt-4.1-mini"]);
}

// ---- strictness -----------------------------------------------------------------------

#[test]
fn a_present_section_requires_model_catalog() {
    for raw in [json!({}), json!({ "vendor": "acme" })] {
        let err = load_err(&raw);

        assert!(
            matches!(err, StaticModelPolicyConfigError::Parse(_)),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("model_catalog"),
            "the error names the missing key: {err}"
        );
    }
}

#[test]
fn unknown_top_level_keys_are_rejected() {
    let err = load_err(&json!({ "model_catalog": [], "bogus_key": 1 }));

    assert!(
        matches!(err, StaticModelPolicyConfigError::Parse(_)),
        "got {err:?}"
    );
    assert!(err.to_string().contains("bogus_key"), "{err}");
}

#[test]
fn unknown_keys_inside_kill_switches_are_rejected() {
    let err = load_err(&json!({
        "model_catalog": [],
        "kill_switches": { "disable_web_serch": true }
    }));

    assert!(
        matches!(err, StaticModelPolicyConfigError::Parse(_)),
        "got {err:?}"
    );
    assert!(err.to_string().contains("disable_web_serch"), "{err}");
}

#[test]
fn unknown_keys_inside_nested_sdk_types_are_ignored() {
    let cfg = load_ok(&json!({
        "model_catalog": [with(entry("gpt-5.2"), "future_field", json!(true))],
        "default_standard_limits": {
            "limit_daily_credits_micro": 1,
            "limit_monthly_credits_micro": 2,
            "future_field": true
        }
    }));

    assert_eq!(cfg.model_catalog.len(), 1);
    assert_eq!(cfg.default_standard_limits.limit_daily_credits_micro, 1);
}

#[test]
fn a_wrongly_typed_value_is_a_parse_error() {
    load_ok(&json!({ "model_catalog": [], "priority": 5 }));

    let err = load_err(&json!({ "model_catalog": [], "priority": "high" }));

    assert!(
        matches!(err, StaticModelPolicyConfigError::Parse(_)),
        "got {err:?}"
    );
}

#[test]
fn an_unknown_tier_is_a_parse_error() {
    load_ok(&section(&[entry("gpt-5.2")]));

    let err = load_err(&section(&[with(entry("gpt-5.2"), "tier", json!("gold"))]));

    assert!(
        matches!(err, StaticModelPolicyConfigError::Parse(_)),
        "got {err:?}"
    );
}

// ---- catalog validation ------------------------------------------------------------------

fn assert_invalid_entry(err: &StaticModelPolicyConfigError, model_id: &str, field: &str) {
    match err {
        StaticModelPolicyConfigError::InvalidEntry {
            model_id: id,
            field: f,
            ..
        } => {
            assert_eq!(id, model_id);
            assert!(f.ends_with(field), "field `{f}` should end with `{field}`");
        }
        other @ StaticModelPolicyConfigError::Parse(_) => {
            panic!("expected InvalidEntry, got {other:?}")
        }
    }
    let text = err.to_string();
    assert!(
        text.contains(model_id),
        "message names the model id: {text}"
    );
    assert!(text.contains(field), "message names the field: {text}");
}

#[test]
fn credit_multipliers_outside_the_allowed_range_fail_init() {
    for (field, bad) in [
        ("input_tokens_credit_multiplier_micro", json!(0)),
        (
            "input_tokens_credit_multiplier_micro",
            json!(10_000_000_001_u64),
        ),
        ("output_tokens_credit_multiplier_micro", json!(0)),
        (
            "output_tokens_credit_multiplier_micro",
            json!(10_000_000_001_u64),
        ),
    ] {
        let err = load_err(&section(&[with(entry("gpt-5.2"), field, bad.clone())]));

        assert_invalid_entry(&err, "gpt-5.2", field);
    }
}

#[test]
fn credit_multipliers_at_the_range_bounds_are_valid() {
    for bound in [1_u64, 10_000_000_000] {
        let raw = section(&[with(
            with(
                entry("gpt-5.2"),
                "input_tokens_credit_multiplier_micro",
                json!(bound),
            ),
            "output_tokens_credit_multiplier_micro",
            json!(bound),
        )]);

        assert_eq!(load_ok(&raw).model_catalog.len(), 1, "bound {bound}");
    }
}

#[test]
fn a_zero_bytes_per_token_conservative_fails_init() {
    let raw = section(&[with(
        entry("gpt-5.2"),
        "estimation_budgets",
        json!({ "bytes_per_token_conservative": 0 }),
    )]);

    let err = load_err(&raw);

    assert_invalid_entry(&err, "gpt-5.2", "bytes_per_token_conservative");
}

#[test]
fn the_error_names_the_offending_entry_of_several() {
    let raw = section(&[
        entry("good-model"),
        with(
            entry("bad-model"),
            "output_tokens_credit_multiplier_micro",
            json!(0),
        ),
    ]);

    let err = load_err(&raw);

    assert_invalid_entry(&err, "bad-model", "output_tokens_credit_multiplier_micro");
}
