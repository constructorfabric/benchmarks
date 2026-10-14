//! The values the two background handlers read: the `thread_summary_worker` and `cleanup_worker`
//! keys, the empty summary model id that means `gpt-4.1-mini`, the provider phase bound of the
//! summary delivery (`claim_timeout_secs` minus 10 s), the deprecated keys that change nothing and
//! the fixed values that stay out of the configuration.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-configuration` (`inst-cf-summary`,
//! `inst-cf-summary-elsewhere`, `inst-cf-cleanup`, `inst-cf-outbox`, `inst-cf-catalog`,
//! `inst-cf-fixed`, `inst-cf-return`), the keys of
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution` (`inst-mr-model-id`)
//! and `cpt-cf-mini-chat-algo-background-maintenance-handler-wiring` (`inst-hw-enabled-flag`);
//! definitions of done `cpt-cf-mini-chat-dod-background-maintenance-observability-config` and
//! `cpt-cf-mini-chat-dod-background-maintenance-handler-wiring`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::{Value, json};

use super::{
    CleanupSettings, DEFAULT_SUMMARY_MODEL_ID, DELIVERY_BUDGET, MAX_ERROR_BODY_BYTES,
    MAX_PROMPT_TOO_LONG_RETRIES, MAX_SUCCESS_BODY_BYTES, PROVIDER_CALL_DEADLINE,
    SUMMARY_LEASE_MARGIN, SYSTEM_SUBJECT_ID, SummaryWorkerSettings,
};
use crate::config::MiniChatConfig;
use crate::infra::outbox::queue_specs;
use crate::test_support::minimal_gear_config;

fn config_with(patch: &Value) -> MiniChatConfig {
    let mut config = minimal_gear_config();
    for (key, value) in patch.as_object().unwrap() {
        config[key] = value.clone();
    }
    MiniChatConfig::from_value(config).expect("valid config")
}

#[test]
fn the_summary_keys_are_read_from_thread_summary_worker() {
    // inst-cf-summary
    let config = config_with(&json!({ "thread_summary_worker": {
        "max_attempts": 7, "summary_model_id": "my-summarizer",
        "summary_system_prompt": "Be brief.", "message_content_limit": 123,
        "claim_timeout_secs": 90
    }}));
    let settings = SummaryWorkerSettings::from_config(&config);
    assert_eq!(settings.max_attempts, 7);
    assert_eq!(settings.summary_model_id, "my-summarizer");
    assert_eq!(settings.summary_system_prompt, "Be brief.");
    assert_eq!(settings.message_content_limit, 123);
    assert_eq!(settings.claim_timeout, Duration::from_secs(90));
}

#[test]
fn the_defaults_are_three_attempts_4000_characters_and_a_300_second_claim() {
    // inst-cf-summary
    let settings = SummaryWorkerSettings::from_config(&config_with(&json!({})));
    assert_eq!(settings.max_attempts, 3);
    assert_eq!(settings.message_content_limit, 4000);
    assert_eq!(settings.claim_timeout, Duration::from_secs(300));
    assert_eq!(
        settings.summary_system_prompt,
        crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT
    );
}

#[test]
fn an_empty_summary_model_id_means_gpt_4_1_mini() {
    // inst-mr-model-id, inst-cf-summary
    assert_eq!(DEFAULT_SUMMARY_MODEL_ID, "gpt-4.1-mini");
    let empty = SummaryWorkerSettings::from_config(&config_with(&json!({})));
    assert_eq!(empty.summary_model_id, "");
    assert_eq!(empty.model_id(), "gpt-4.1-mini");
    let set = SummaryWorkerSettings::from_config(&config_with(&json!({
        "thread_summary_worker": { "summary_model_id": "other-model" }
    })));
    assert_eq!(set.model_id(), "other-model");
}

#[test]
fn the_provider_phase_ends_ten_seconds_before_the_lease() {
    // inst-cf-summary (`claim_timeout_secs`), inst-cf-fixed (the 10 s margin below the lease)
    assert_eq!(SUMMARY_LEASE_MARGIN, Duration::from_secs(10));
    let settings = SummaryWorkerSettings::from_config(&config_with(&json!({})));
    assert_eq!(settings.provider_phase_bound(), Duration::from_secs(290));
    let short = SummaryWorkerSettings::from_config(&config_with(&json!({
        "thread_summary_worker": { "claim_timeout_secs": 30 }
    })));
    assert_eq!(short.provider_phase_bound(), Duration::from_secs(20));
}

#[test]
fn a_zero_content_limit_means_no_truncation_and_is_kept() {
    // inst-cf-summary: 0 means no truncation
    let settings = SummaryWorkerSettings::from_config(&config_with(&json!({
        "thread_summary_worker": { "message_content_limit": 0 }
    })));
    assert_eq!(settings.message_content_limit, 0);
}

#[test]
fn the_enabled_flag_the_threshold_and_the_deprecated_keys_change_nothing() {
    // inst-cf-summary-elsewhere, inst-hw-enabled-flag: `enabled` and `compression_threshold_pct`
    // belong to the trigger, `reconcile_interval_secs` is deprecated
    let base = SummaryWorkerSettings::from_config(&config_with(&json!({})));
    let other = SummaryWorkerSettings::from_config(&config_with(&json!({
        "thread_summary_worker": {
            "enabled": false, "compression_threshold_pct": 55, "reconcile_interval_secs": 5
        }
    })));
    assert_eq!(base, other);
}

#[test]
fn the_cleanup_budget_is_cleanup_worker_max_attempts() {
    // inst-cf-cleanup
    assert_eq!(
        CleanupSettings::from_config(&config_with(&json!({}))).max_attempts,
        5
    );
    let set = CleanupSettings::from_config(&config_with(&json!({
        "cleanup_worker": { "max_attempts": 2 }
    })));
    assert_eq!(set.max_attempts, 2);
}

#[test]
fn the_deprecated_cleanup_keys_change_nothing() {
    // inst-cf-cleanup: `enabled`, `poll_interval_secs`, `reconcile_interval_secs`,
    // `stale_in_progress_timeout_secs` and `batch_size` have no effect (ADR-0010)
    let base = CleanupSettings::from_config(&config_with(&json!({})));
    let other = CleanupSettings::from_config(&config_with(&json!({
        "cleanup_worker": {
            "enabled": false, "poll_interval_secs": 1, "reconcile_interval_secs": 2,
            "stale_in_progress_timeout_secs": 3, "batch_size": 4
        }
    })));
    assert_eq!(base, other);
}

#[test]
fn the_queue_names_come_from_the_outbox_section() {
    // inst-cf-outbox: `outbox.thread_summary_queue_name`, `outbox.chat_cleanup_queue_name`,
    // `outbox.queue_name` and `outbox.num_partitions` through the foundation's registrations
    let defaults = queue_specs(&config_with(&json!({})).outbox, 300);
    let names: Vec<&str> = defaults.iter().map(|spec| spec.name.as_str()).collect();
    assert!(names.contains(&"mini-chat.thread_summary"), "{names:?}");
    assert!(names.contains(&"mini-chat.chat_cleanup"), "{names:?}");
    assert!(names.contains(&"mini-chat.usage_snapshot"), "{names:?}");
    let custom = queue_specs(
        &config_with(&json!({ "outbox": {
            "thread_summary_queue_name": "custom.summary",
            "chat_cleanup_queue_name": "custom.chat_cleanup",
            "queue_name": "custom.usage", "num_partitions": 8
        }}))
        .outbox,
        300,
    );
    let names: Vec<&str> = custom.iter().map(|spec| spec.name.as_str()).collect();
    assert!(names.contains(&"custom.summary"), "{names:?}");
    assert!(names.contains(&"custom.chat_cleanup"), "{names:?}");
    assert!(names.contains(&"custom.usage"), "{names:?}");
    assert!(custom.iter().all(|spec| spec.partitions == 8));
}

#[test]
fn the_catalog_fields_are_the_ones_of_the_model_entry() {
    // inst-cf-catalog: `provider_model_id`, `provider_id`, `enabled`, `context_window`,
    // `max_output_tokens`, `max_input_tokens`, `estimation_budgets.bytes_per_token_conservative`
    // and `thread_summary_prompt` exist on the catalog entry the handler reads
    let entry = crate::test_support::maintenance::summary_entry(
        "m",
        "azure_openai",
        &json!({ "thread_summary_prompt": "P", "context_window": 10, "max_output_tokens": 2,
                  "max_input_tokens": 7 }),
    );
    assert_eq!(entry.provider_model_id, "provider-m");
    assert_eq!(entry.provider_id, "azure_openai");
    assert!(entry.enabled);
    assert_eq!(entry.context_window, 10);
    assert_eq!(entry.max_output_tokens, 2);
    assert_eq!(entry.max_input_tokens, 7);
    assert_eq!(entry.thread_summary_prompt, "P");
    assert!(entry.estimation_budgets.bytes_per_token_conservative >= 1);
}

#[test]
fn the_fixed_values_are_not_configuration() {
    // inst-cf-fixed, inst-cf-return
    assert_eq!(MAX_PROMPT_TOO_LONG_RETRIES, 2);
    assert_eq!(PROVIDER_CALL_DEADLINE, Duration::from_secs(5));
    assert_eq!(DELIVERY_BUDGET, Duration::from_secs(20));
    assert_eq!(MAX_ERROR_BODY_BYTES, 16 * 1024);
    assert_eq!(MAX_SUCCESS_BODY_BYTES, 1024 * 1024);
    assert_eq!(
        SYSTEM_SUBJECT_ID.to_string(),
        "11111111-6a88-4768-9dfc-6bcd5187d9ed"
    );
    // both deadlines sit inside the 30 s default lease of chat cleanup
    assert!(DELIVERY_BUDGET + PROVIDER_CALL_DEADLINE < Duration::from_secs(30));
}

#[test]
fn a_provider_kind_other_than_the_four_adapters_is_rejected_at_startup() {
    // inst-ad-default: unreachable at the summary call, because the configuration validation of
    // the foundation rejects any other `kind`
    let mut config = minimal_gear_config();
    config["providers"] = json!({
        "gemini": { "kind": "gemini_generate", "host": "gemini.example.com" }
    });
    assert!(MiniChatConfig::from_value(config).is_err());
}
