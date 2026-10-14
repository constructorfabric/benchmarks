//! Startup configuration errors of the streaming feature, through the gear `init` and the config
//! validation: any provider `kind` other than the four adapter kinds is rejected at startup, the
//! removed key `streaming.web_search_context_size` is an unknown-key startup error (the search
//! context size comes from the catalog), and the configuration algorithm returns the values it
//! reads.
//!
//! Slice S3-completion of `cpt-cf-mini-chat-feature-message-streaming`: algorithms `adapter-kinds`
//! (`inst-kind-default`) and `configuration` (`inst-cfg-removed`, `inst-cfg-return`); definitions
//! of done `cpt-cf-mini-chat-dod-message-streaming-provider-adapters` and
//! `cpt-cf-mini-chat-dod-message-streaming-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use toolkit::Gear;

use super::MiniChatGear;
use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::streaming::StreamSettings;
use crate::infra::storage::migrations::all_migrations;
use crate::test_support::s2::{Platform, RecordingOagw, ScriptedAuthN, platform};
use crate::test_support::{gear_ctx, minimal_gear_config, sqlite_db};

fn with_provider(kind: &Value) -> Value {
    let mut config = minimal_gear_config();
    config["providers"] = json!({
        "good": { "kind": "openai_responses", "host": "api.openai.com", "storage_kind": "openai" },
        "bad": { "kind": kind, "host": "api.example.com", "storage_kind": "openai" },
    });
    config
}

/// Run the gear `init` with `config`; `Ok` with the gear when startup accepts it.
async fn init(config: Value) -> Result<MiniChatGear, String> {
    let platform: Platform = platform(ScriptedAuthN::ok(), RecordingOagw::accepting());
    let db = sqlite_db(all_migrations()).await;
    let ctx = gear_ctx(
        "mini-chat",
        Some(config),
        Arc::clone(&platform.hub),
        Some(db.db.clone()),
    );
    let gear = MiniChatGear::default();
    match gear.init(&ctx).await {
        Ok(()) => Ok(gear),
        Err(error) => Err(format!("{error:#}")),
    }
}

// ── inst-kind-default ────────────────────────────────────────────────────────

#[tokio::test]
async fn startup_accepts_exactly_the_four_adapter_kinds() {
    for kind in [
        "openai_responses",
        "openai_chat_completions",
        "vllm_responses",
        "anthropic_messages",
    ] {
        let gear = init(with_provider(&json!(kind)))
            .await
            .unwrap_or_else(|error| panic!("`{kind}` must start: {error}"));
        assert!(gear.provider_registry().is_some(), "{kind}");
    }
}

#[tokio::test]
async fn startup_rejects_every_unknown_provider_kind_and_builds_nothing() {
    let unknown = [
        json!("gemini"),
        json!("openai"),
        json!("azure_openai"),
        json!("anthropic"),
        json!("vllm"),
        json!("responses"),
        json!("openai_completions"),
        json!("OpenAI_Responses"),
        json!("ANTHROPIC_MESSAGES"),
        json!("openai-responses"),
        json!(" openai_responses"),
        json!("openai_responses "),
        json!(""),
        json!(null),
        json!(1),
        json!(true),
        json!(["openai_responses"]),
        json!({ "kind": "openai_responses" }),
    ];
    for kind in unknown {
        let error = init(with_provider(&kind))
            .await
            .err()
            .unwrap_or_else(|| panic!("the kind {kind} must fail startup"));
        assert!(!error.is_empty(), "{kind}");
    }
}

#[test]
fn the_config_validation_names_the_rejected_kind_or_the_key() {
    for kind in ["gemini", "OpenAI_Responses", "openai"] {
        let message = MiniChatConfig::from_value(with_provider(&json!(kind)))
            .expect_err("an unknown kind is rejected")
            .to_string();
        assert!(
            message.contains(kind) || message.contains("kind"),
            "{kind}: {message}"
        );
    }
}

#[test]
fn the_rejected_kind_is_rejected_in_any_provider_entry_not_only_the_first() {
    let mut config = minimal_gear_config();
    config["providers"] = json!({
        "a": { "kind": "openai_responses", "host": "a.example.com", "storage_kind": "openai" },
        "b": { "kind": "vllm_responses", "host": "b.example.com", "storage_kind": "openai" },
        "c": { "kind": "mistral", "host": "c.example.com", "storage_kind": "openai" },
    });
    assert!(MiniChatConfig::from_value(config).is_err());
}

#[test]
fn the_provider_kind_enumeration_has_exactly_the_four_adapter_kinds() {
    // an exhaustive match: a fifth variant fails to compile here and needs an adapter first
    let all = [
        ProviderKind::OpenaiResponses,
        ProviderKind::OpenaiChatCompletions,
        ProviderKind::VllmResponses,
        ProviderKind::AnthropicMessages,
    ];
    for kind in all {
        match kind {
            ProviderKind::OpenaiResponses
            | ProviderKind::OpenaiChatCompletions
            | ProviderKind::VllmResponses
            | ProviderKind::AnthropicMessages => {}
        }
    }
}

// ── inst-cfg-removed ─────────────────────────────────────────────────────────

#[test]
fn the_removed_streaming_web_search_context_size_is_a_startup_error_for_any_value() {
    for value in [
        json!("low"),
        json!("medium"),
        json!("high"),
        json!(""),
        json!(null),
        json!(1),
        json!({}),
    ] {
        let mut config = minimal_gear_config();
        config["streaming"] = json!({ "web_search_context_size": value });
        let message = MiniChatConfig::from_value(config)
            .expect_err("the removed key must be rejected")
            .to_string();
        assert!(
            message.contains("web_search_context_size"),
            "{value}: {message}"
        );
    }
}

#[test]
fn the_removed_key_is_rejected_next_to_valid_streaming_keys() {
    let mut config = minimal_gear_config();
    config["streaming"] = json!({
        "sse_ping_interval_seconds": 20,
        "sse_channel_capacity": 32,
        "max_output_tokens": 4096,
        "web_search_context_size": "high",
    });
    let message = MiniChatConfig::from_value(config)
        .expect_err("the removed key must be rejected")
        .to_string();
    assert!(message.contains("web_search_context_size"), "{message}");
    assert!(
        message.contains("catalog") || message.contains("removed") || message.contains("unknown"),
        "{message}"
    );
}

#[test]
fn the_same_streaming_section_without_the_removed_key_is_accepted() {
    let mut config = minimal_gear_config();
    config["streaming"] = json!({
        "sse_ping_interval_seconds": 20,
        "sse_channel_capacity": 32,
        "max_output_tokens": 4096,
    });
    let config = MiniChatConfig::from_value(config).expect("valid");
    assert_eq!(config.streaming.max_output_tokens, 4096);
}

#[tokio::test]
async fn the_gear_init_fails_on_the_removed_key() {
    let mut config = minimal_gear_config();
    config["streaming"] = json!({ "web_search_context_size": "low" });
    let error = init(config)
        .await
        .err()
        .expect("init must fail on the removed key");
    assert!(error.contains("web_search_context_size"), "{error}");
}

// ── inst-cfg-return ──────────────────────────────────────────────────────────

#[test]
fn the_configuration_returns_the_context_and_rag_values_the_feature_reads() {
    let defaults =
        StreamSettings::from_config(&MiniChatConfig::from_value(minimal_gear_config()).unwrap());
    assert_eq!(defaults.recent_messages_limit, 10);
    assert_eq!(defaults.max_images_per_message, 4);
    assert_eq!(defaults.max_documents_per_chat, 50);
    assert_eq!(defaults.sse_ping_interval, Duration::from_secs(15));
    assert!(!defaults.web_search_guard.is_empty());
    assert!(!defaults.file_search_guard.is_empty());

    let mut raw = minimal_gear_config();
    raw["context"] = json!({
        "recent_messages_limit": 3,
        "web_search_guard": "WEB",
        "file_search_guard": "FILE",
    });
    raw["rag"] = json!({ "max_images_per_message": 2, "max_documents_per_chat": 7 });
    raw["streaming"] = json!({ "sse_ping_interval_seconds": 9 });
    let custom = StreamSettings::from_config(&MiniChatConfig::from_value(raw).unwrap());
    assert_eq!(custom.recent_messages_limit, 3);
    assert_eq!(custom.web_search_guard, "WEB");
    assert_eq!(custom.file_search_guard, "FILE");
    assert_eq!(custom.max_images_per_message, 2);
    assert_eq!(custom.max_documents_per_chat, 7);
    assert_eq!(custom.sse_ping_interval, Duration::from_secs(9));
}

#[test]
fn the_configuration_returns_the_streaming_tool_watchdog_and_summary_values() {
    let mut raw = minimal_gear_config();
    raw["streaming"] = json!({ "sse_channel_capacity": 48, "max_output_tokens": 2048 });
    raw["quota"] = json!({
        "web_search_max_calls_per_message": 4,
        "code_interpreter_max_calls_per_message": 6,
    });
    raw["orphan_watchdog"] =
        json!({ "enabled": false, "timeout_secs": 600, "scan_interval_secs": 30 });
    raw["thread_summary_worker"] = json!({ "enabled": false, "compression_threshold_pct": 70 });
    let config = MiniChatConfig::from_value(raw).unwrap();
    assert_eq!(config.streaming.sse_channel_capacity, 48);
    assert_eq!(config.streaming.max_output_tokens, 2048);
    assert_eq!(config.quota.web_search_max_calls_per_message, 4);
    assert_eq!(config.quota.code_interpreter_max_calls_per_message, 6);
    assert!(!config.orphan_watchdog.enabled);
    assert_eq!(config.orphan_watchdog.timeout_secs, 600);
    assert_eq!(config.orphan_watchdog.scan_interval_secs, 30);
    assert!(!config.thread_summary_worker.enabled);
    assert_eq!(config.thread_summary_worker.compression_threshold_pct, 70);
}

#[test]
fn the_tool_limits_must_be_greater_than_zero() {
    for key in [
        "web_search_max_calls_per_message",
        "code_interpreter_max_calls_per_message",
    ] {
        let mut raw = minimal_gear_config();
        raw["quota"] = json!({ key: 0 });
        assert!(MiniChatConfig::from_value(raw).is_err(), "{key} = 0");
    }
}
