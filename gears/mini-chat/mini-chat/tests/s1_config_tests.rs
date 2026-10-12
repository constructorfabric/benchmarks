#![allow(clippy::unwrap_used, clippy::expect_used)]

//! S1 gear configuration tests (DESIGN Appendix B.1, B.4-B.9): serde defaults,
//! unknown-key handling and the `MiniChatConfig::validate` startup rules.
//!
//! Every case starts from the smallest valid configuration (only the required
//! S2S credentials) and patches the keys under test into it.

use mini_chat::config::MiniChatConfig;
use serde_json::{Map, Value, json};

const BUILTIN_SUMMARY_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

const WORKER_SECTIONS: [&str; 4] = [
    "orphan_watchdog",
    "upload_reaper",
    "thread_summary_worker",
    "cleanup_worker",
];

// ── Helpers ──────────────────────────────────────────────────────────────

fn merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(dst), Value::Object(src)) => {
            for (key, value) in src {
                merge(dst.entry(key).or_insert(Value::Null), value);
            }
        }
        (slot, value) => *slot = value,
    }
}

fn base_json() -> Value {
    json!({ "client_credentials": { "client_id": "a", "client_secret": "b" } })
}

fn parse(patch: Value) -> Result<MiniChatConfig, serde_json::Error> {
    let mut raw = base_json();
    merge(&mut raw, patch);
    serde_json::from_value(raw)
}

fn parsed(patch: Value) -> MiniChatConfig {
    parse(patch).expect("config must deserialize")
}

fn validate(patch: Value) -> Result<(), String> {
    parsed(patch).validate()
}

/// `{ section: { key: value } }`
fn section_patch(section: &str, key: &str, value: Value) -> Value {
    let mut inner = Map::new();
    inner.insert(key.to_owned(), value);
    let mut outer = Map::new();
    outer.insert(section.to_owned(), Value::Object(inner));
    Value::Object(outer)
}

fn assert_bounds(section: &str, key: &str, accepted: &[Value], rejected: &[Value]) {
    for value in accepted {
        let result = validate(section_patch(section, key, value.clone()));
        assert!(
            result.is_ok(),
            "{section}.{key} = {value} must be accepted: {result:?}"
        );
    }
    for value in rejected {
        let result = validate(section_patch(section, key, value.clone()));
        assert!(
            result.is_err(),
            "{section}.{key} = {value} must be rejected"
        );
    }
}

fn assert_positive(section: &str, key: &str) {
    assert_bounds(section, key, &[json!(1)], &[json!(0)]);
}

/// Normalized enum variant name: `OpenAiResponses` / `OpenaiResponses` -> `openairesponses`.
fn variant_name<T: std::fmt::Debug>(value: &T) -> String {
    format!("{value:?}").to_lowercase().replace('_', "")
}

fn openai_entry() -> Value {
    json!({ "kind": "openai_responses", "host": "api.openai.com", "storage_kind": "openai" })
}

fn openai_entry_with(extra: Value) -> Value {
    let mut entry = openai_entry();
    merge(&mut entry, extra);
    entry
}

/// `{ "providers": { "p": entry } }`
fn single_provider(entry: Value) -> Value {
    section_patch("providers", "p", entry)
}

fn azure_entry(api_version: Option<&str>) -> Value {
    let mut entry = openai_entry_with(json!({
        "host": "my-resource.openai.azure.com",
        "storage_kind": "azure",
        "api_path": "/openai/v1/responses"
    }));
    if let Some(version) = api_version {
        merge(&mut entry, json!({ "api_version": version }));
    }
    entry
}

fn tenant_override(override_entry: &Value) -> Value {
    single_provider(openai_entry_with(json!({
        "tenant_overrides": { "6f1c2d3e-4a5b-4c6d-8e7f-9a0b1c2d3e4f": override_entry }
    })))
}

/// The `mini-chat.config` section of `config/mini-chat.yaml` as JSON, with a
/// literal host in place of `${AZURE_OPENAI_API_HOST}` (validation runs after
/// `${VAR}` expansion).
fn yaml_gear_section() -> Value {
    json!({
        "vendor": "constructorfabric",
        "orphan_watchdog": {
            "scan_interval_secs": 10,
            "timeout_secs": 90
        },
        "client_credentials": {
            "client_id": "mini-chat",
            "client_secret": "mini-chat-dev-secret"
        },
        "thread_summary_worker": {
            "enabled": true,
            "summary_model_id": "gpt-4.1-mini",
            "compression_threshold_pct": 80,
            "summary_system_prompt": BUILTIN_SUMMARY_PROMPT
        },
        "providers": {
            "azure_openai": {
                "kind": "openai_responses",
                "storage_kind": "azure",
                "host": "my-resource.openai.azure.com",
                "api_path": "/openai/v1/responses",
                "api_version": "2025-03-01-preview",
                "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "auth_config": {
                    "header": "api-key",
                    "prefix": "",
                    "secret_ref": "azure-openai-key"
                }
            }
        }
    })
}

// ── Defaults ─────────────────────────────────────────────────────────────

#[test]
fn defaults_top_level() {
    let cfg = parsed(json!({}));

    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.client_credentials.client_id, "a");
    assert!(cfg.metrics.prefix.is_empty());
}

#[test]
fn defaults_single_openai_provider() {
    let cfg = parsed(json!({}));

    assert_eq!(cfg.providers.len(), 1);
    let openai = cfg
        .providers
        .get("openai")
        .expect("default provider `openai`");
    assert_eq!(variant_name(&openai.kind), "openairesponses");
    assert_eq!(openai.host, "api.openai.com");
    assert_eq!(variant_name(&openai.storage_kind), "openai");
    assert_eq!(openai.api_path, "/v1/responses");
}

#[test]
fn defaults_streaming() {
    let cfg = parsed(json!({}));

    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
    assert_eq!(cfg.streaming.max_output_tokens, 32_768);
}

#[test]
fn defaults_estimation_budgets_floor() {
    let cfg = parsed(json!({}));

    assert_eq!(cfg.estimation_budgets.minimal_generation_floor, 50);
}

#[test]
fn defaults_quota() {
    let cfg = parsed(json!({}));
    let quota = &cfg.quota;

    assert!(
        (quota.overshoot_tolerance_factor - 1.10).abs() < 1e-9,
        "overshoot_tolerance_factor: {}",
        quota.overshoot_tolerance_factor
    );
    assert_eq!(quota.warning_threshold_pct, 80);
    assert_eq!(quota.web_search_max_calls_per_message, 2);
    assert_eq!(quota.web_search_daily_quota, 75);
    assert_eq!(quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(quota.code_interpreter_daily_quota, 50);
}

#[test]
fn defaults_outbox() {
    let cfg = parsed(json!({}));
    let outbox = &cfg.outbox;

    assert_eq!(outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(outbox.cleanup_queue_name, "mini-chat.attachment_cleanup");
    assert_eq!(outbox.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(outbox.thread_summary_queue_name, "mini-chat.thread_summary");
    assert_eq!(outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(outbox.num_partitions, 4);
}

#[test]
fn defaults_context() {
    let cfg = parsed(json!({}));

    assert_eq!(cfg.context.recent_messages_limit, 10);
    assert!(!cfg.context.web_search_guard.trim().is_empty());
    assert!(!cfg.context.file_search_guard.trim().is_empty());
}

#[test]
fn defaults_rag() {
    let cfg = parsed(json!({}));
    let rag = &cfg.rag;

    assert_eq!(rag.max_documents_per_chat, 50);
    assert_eq!(rag.max_total_upload_mb_per_chat, 100);
    assert!(rag.allow_csv_upload);
    assert_eq!(rag.max_concurrent_uploads, 10);
    assert_eq!(rag.uploaded_file_max_size_kb, 25_600);
    assert_eq!(rag.uploaded_image_max_size_kb, 5_120);
    assert_eq!(rag.max_images_per_message, 4);
}

#[test]
fn defaults_thumbnail() {
    let cfg = parsed(json!({}));
    let thumbnail = &cfg.thumbnail;

    assert_eq!(thumbnail.width, 128);
    assert_eq!(thumbnail.height, 128);
    assert_eq!(thumbnail.max_bytes, 131_072);
    assert_eq!(thumbnail.max_pixels, 100_000_000);
    assert_eq!(thumbnail.max_decode_bytes, 33_554_432);
}

#[test]
fn defaults_knowledge_search() {
    let cfg = parsed(json!({}));
    let knowledge = &cfg.knowledge_search;

    assert!(!knowledge.enabled);
    assert!(knowledge.vector_store_id.is_none());
    assert!(knowledge.provider_id.is_none());
    assert_eq!(knowledge.max_calls_per_message, 3);
    assert_eq!(knowledge.top_k, 5);
    assert_eq!(knowledge.max_chunk_chars, 2000);
    assert!(!knowledge.guard.trim().is_empty());
}

#[test]
fn defaults_orphan_watchdog() {
    let cfg = parsed(json!({}));

    assert!(cfg.orphan_watchdog.enabled);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 60);
}

#[test]
fn defaults_upload_reaper() {
    let cfg = parsed(json!({}));

    assert!(cfg.upload_reaper.enabled);
    assert_eq!(cfg.upload_reaper.scan_interval_secs, 60);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 300);
}

#[test]
fn defaults_thread_summary_worker() {
    let cfg = parsed(json!({}));
    let worker = &cfg.thread_summary_worker;

    assert!(worker.enabled);
    assert_eq!(worker.claim_timeout_secs, 300);
    assert_eq!(worker.max_attempts, 3);
    assert_eq!(worker.compression_threshold_pct, 80);
    assert!(worker.summary_model_id.is_empty());
    assert_eq!(worker.summary_system_prompt, BUILTIN_SUMMARY_PROMPT);
    assert_eq!(worker.message_content_limit, 4000);
    assert_eq!(worker.reconcile_interval_secs, 60);
}

#[test]
fn defaults_cleanup_worker() {
    let cfg = parsed(json!({}));
    let worker = &cfg.cleanup_worker;

    assert_eq!(worker.max_attempts, 5);
    assert!(worker.enabled);
    assert_eq!(worker.poll_interval_secs, 60);
    assert_eq!(worker.reconcile_interval_secs, 300);
    assert_eq!(worker.stale_in_progress_timeout_secs, 900);
    assert_eq!(worker.batch_size, 32);
}

#[test]
fn defaults_with_credentials_validate() {
    assert_eq!(validate(json!({})), Ok(()));
}

#[test]
fn defaults_without_credentials_fail_validation() {
    let cfg: MiniChatConfig =
        serde_json::from_value(json!({})).expect("every key has a serde default");

    assert!(cfg.validate().is_err(), "client credentials are required");
}

#[test]
fn default_impl_fails_validation_without_credentials() {
    assert!(MiniChatConfig::default().validate().is_err());
}

// ── Required values ──────────────────────────────────────────────────────

#[test]
fn validate_rejects_empty_client_id() {
    let result = validate(json!({ "client_credentials": { "client_id": "" } }));

    assert!(result.is_err());
}

#[test]
fn validate_rejects_empty_client_secret() {
    let result = validate(json!({ "client_credentials": { "client_secret": "" } }));

    assert!(result.is_err());
}

#[test]
fn validate_rejects_empty_vendor() {
    let result = validate(json!({ "vendor": "" }));

    assert!(result.is_err());
}

#[test]
fn validate_rejects_empty_queue_names() {
    for key in [
        "queue_name",
        "cleanup_queue_name",
        "chat_cleanup_queue_name",
        "thread_summary_queue_name",
        "audit_queue_name",
    ] {
        let result = validate(section_patch("outbox", key, json!("")));

        assert!(result.is_err(), "empty outbox.{key} must be rejected");
    }
}

// ── Ranged keys ──────────────────────────────────────────────────────────

#[test]
fn validate_sse_ping_interval_range() {
    assert_bounds(
        "streaming",
        "sse_ping_interval_seconds",
        &[json!(5), json!(60)],
        &[json!(0), json!(4), json!(61)],
    );
}

#[test]
fn validate_sse_channel_capacity_range() {
    assert_bounds(
        "streaming",
        "sse_channel_capacity",
        &[json!(16), json!(64)],
        &[json!(0), json!(15), json!(65)],
    );
}

#[test]
fn validate_overshoot_tolerance_factor_range() {
    assert_bounds(
        "quota",
        "overshoot_tolerance_factor",
        &[json!(1.0), json!(1.5)],
        &[json!(0.99), json!(1.51), json!(0.0)],
    );
}

#[test]
fn validate_warning_threshold_pct_range() {
    assert_bounds(
        "quota",
        "warning_threshold_pct",
        &[json!(1), json!(99)],
        &[json!(0), json!(100)],
    );
}

#[test]
fn validate_num_partitions_power_of_two_up_to_64() {
    assert_bounds(
        "outbox",
        "num_partitions",
        &[
            json!(1),
            json!(2),
            json!(4),
            json!(8),
            json!(16),
            json!(32),
            json!(64),
        ],
        &[json!(0), json!(3), json!(6), json!(63), json!(128)],
    );
}

#[test]
fn validate_recent_messages_limit_range() {
    assert_bounds(
        "context",
        "recent_messages_limit",
        &[json!(0), json!(100)],
        &[json!(101)],
    );
}

#[test]
fn validate_orphan_watchdog_timeout_range() {
    assert_bounds(
        "orphan_watchdog",
        "timeout_secs",
        &[json!(90), json!(3600)],
        &[json!(89), json!(3601)],
    );
}

#[test]
fn validate_orphan_watchdog_scan_interval_range() {
    assert_bounds(
        "orphan_watchdog",
        "scan_interval_secs",
        &[json!(1), json!(3600)],
        &[json!(0), json!(3601)],
    );
}

#[test]
fn validate_upload_reaper_scan_interval_range() {
    assert_bounds(
        "upload_reaper",
        "scan_interval_secs",
        &[json!(1), json!(3600)],
        &[json!(0), json!(3601)],
    );
}

#[test]
fn validate_upload_reaper_stale_after_range() {
    assert_bounds(
        "upload_reaper",
        "stale_after_secs",
        &[json!(60), json!(86_400)],
        &[json!(59), json!(86_401)],
    );
}

#[test]
fn validate_thread_summary_claim_timeout_range() {
    assert_bounds(
        "thread_summary_worker",
        "claim_timeout_secs",
        &[json!(30), json!(3600)],
        &[json!(29), json!(3601)],
    );
}

#[test]
fn validate_thread_summary_compression_threshold_range() {
    assert_bounds(
        "thread_summary_worker",
        "compression_threshold_pct",
        &[json!(1), json!(99)],
        &[json!(0), json!(100)],
    );
}

#[test]
fn validate_thread_summary_max_attempts_positive() {
    assert_positive("thread_summary_worker", "max_attempts");
}

#[test]
fn validate_cleanup_worker_max_attempts_positive() {
    assert_positive("cleanup_worker", "max_attempts");
}

#[test]
fn validate_rag_max_concurrent_uploads_range() {
    assert_bounds(
        "rag",
        "max_concurrent_uploads",
        &[json!(1), json!(256)],
        &[json!(0), json!(257)],
    );
}

#[test]
fn validate_rag_counts_positive() {
    for key in [
        "max_documents_per_chat",
        "max_total_upload_mb_per_chat",
        "uploaded_file_max_size_kb",
        "uploaded_image_max_size_kb",
        "max_images_per_message",
    ] {
        assert_positive("rag", key);
    }
}

#[test]
fn validate_thumbnail_values_positive() {
    for key in [
        "width",
        "height",
        "max_bytes",
        "max_pixels",
        "max_decode_bytes",
    ] {
        assert_positive("thumbnail", key);
    }
}

#[test]
fn validate_quota_tool_counts_positive() {
    for key in [
        "web_search_max_calls_per_message",
        "web_search_daily_quota",
        "code_interpreter_max_calls_per_message",
        "code_interpreter_daily_quota",
    ] {
        assert_positive("quota", key);
    }
}

#[test]
fn validate_minimal_generation_floor_bounds() {
    assert_bounds(
        "estimation_budgets",
        "minimal_generation_floor",
        &[json!(1), json!(32_768)],
        &[json!(0), json!(32_769)],
    );
}

#[test]
fn validate_minimal_generation_floor_follows_streaming_max_output_tokens() {
    let at_cap = validate(json!({
        "streaming": { "max_output_tokens": 100 },
        "estimation_budgets": { "minimal_generation_floor": 100 }
    }));
    let above_cap = validate(json!({
        "streaming": { "max_output_tokens": 100 },
        "estimation_budgets": { "minimal_generation_floor": 101 }
    }));

    assert_eq!(at_cap, Ok(()));
    assert!(above_cap.is_err());
}

#[test]
fn validate_ignores_deprecated_estimation_budget_fields() {
    let result = validate(json!({
        "estimation_budgets": {
            "bytes_per_token_conservative": 0,
            "fixed_overhead_tokens": 0,
            "safety_margin_pct": 0,
            "image_token_budget": 0,
            "tool_surcharge_tokens": 0,
            "web_search_surcharge_tokens": 0,
            "code_interpreter_surcharge_tokens": 0
        }
    }));

    assert_eq!(result, Ok(()));
}

// ── Unknown keys ─────────────────────────────────────────────────────────

#[test]
fn unknown_top_level_mcp_key_is_rejected() {
    let result = parse(json!({ "mcp": { "servers": [] } }));

    assert!(result.is_err(), "`mcp` is not a config key (ADR-0006)");
}

#[test]
fn removed_streaming_web_search_context_size_is_rejected() {
    let result = parse(json!({ "streaming": { "web_search_context_size": "low" } }));

    assert!(result.is_err());
}

#[test]
fn removed_provider_supports_file_search_filters_is_rejected() {
    let result = parse(single_provider(openai_entry_with(json!({
        "supports_file_search_filters": true
    }))));

    assert!(result.is_err());
}

#[test]
fn unknown_key_in_strict_section_is_rejected() {
    for section in [
        "streaming",
        "estimation_budgets",
        "quota",
        "outbox",
        "context",
        "rag",
        "client_credentials",
        "metrics",
        "thumbnail",
        "knowledge_search",
    ] {
        let result = parse(section_patch(section, "not_a_real_key", json!(1)));

        assert!(
            result.is_err(),
            "unknown key in `{section}` must be rejected"
        );
    }
}

#[test]
fn unknown_key_in_tenant_override_is_rejected() {
    let result = parse(tenant_override(&json!({
        "host": "tenant-a.openai.azure.com",
        "api_path": "/v2/responses"
    })));

    assert!(result.is_err());
}

#[test]
fn unknown_key_in_worker_section_is_accepted() {
    for section in WORKER_SECTIONS {
        let result = parse(section_patch(section, "not_a_real_key", json!(1)));

        let cfg = result
            .unwrap_or_else(|err| panic!("unknown key in `{section}` must be ignored, got: {err}"));
        assert_eq!(cfg.validate(), Ok(()), "section `{section}`");
    }
}

// ── Providers: shape ─────────────────────────────────────────────────────

#[test]
fn provider_missing_required_field_is_rejected() {
    for field in ["kind", "host", "storage_kind"] {
        let mut entry = openai_entry();
        entry
            .as_object_mut()
            .expect("entry is an object")
            .remove(field);

        let result = parse(single_provider(entry));

        assert!(
            result.is_err(),
            "provider without `{field}` must be rejected"
        );
    }
}

#[test]
fn provider_accepts_every_adapter_kind() {
    for kind in [
        "openai_responses",
        "openai_chat_completions",
        "vllm_responses",
        "anthropic_messages",
    ] {
        let result = parse(single_provider(openai_entry_with(json!({ "kind": kind }))));

        assert!(result.is_ok(), "kind `{kind}` must deserialize");
    }
}

#[test]
fn provider_rejects_unknown_adapter_kind() {
    let result = parse(single_provider(openai_entry_with(
        json!({ "kind": "gemini" }),
    )));

    assert!(result.is_err());
}

#[test]
fn provider_rejects_unknown_storage_kind() {
    let result = parse(single_provider(openai_entry_with(
        json!({ "storage_kind": "s3" }),
    )));

    assert!(result.is_err());
}

#[test]
fn provider_optional_fields_default() {
    let cfg = parsed(single_provider(openai_entry()));
    let entry = cfg.providers.get("p").expect("provider `p`");

    assert_eq!(entry.api_path, "/v1/responses");
    assert!(entry.port.is_none());
    assert!(!entry.use_http);
    assert!(entry.auth_plugin_type.is_none());
    assert!(entry.auth_config.is_none());
    assert!(entry.storage_backend.is_none());
    assert!(entry.api_version.is_none());
    assert!(entry.rag_provider.is_none());
    assert!(entry.tenant_overrides.is_empty());
}

#[test]
fn configured_providers_replace_the_default_openai_entry() {
    let cfg = parsed(single_provider(openai_entry()));

    assert_eq!(cfg.providers.len(), 1);
    assert!(!cfg.providers.contains_key("openai"));
}

// ── Providers: validation ────────────────────────────────────────────────

#[test]
fn provider_azure_without_api_version_is_rejected() {
    let result = validate(single_provider(azure_entry(None)));

    assert!(result.is_err());
}

#[test]
fn provider_azure_with_blank_api_version_is_rejected() {
    for version in ["", "   "] {
        let result = validate(single_provider(azure_entry(Some(version))));

        assert!(result.is_err(), "api_version {version:?} must be rejected");
    }
}

#[test]
fn provider_azure_with_invalid_api_version_characters_is_rejected() {
    for version in [
        "2025-03-01?x=1",
        "2025 03 01",
        "2025/03/01",
        "2025-03-01&a=b",
    ] {
        let result = validate(single_provider(azure_entry(Some(version))));

        assert!(result.is_err(), "api_version {version:?} must be rejected");
    }
}

#[test]
fn provider_azure_with_valid_api_version_is_accepted() {
    let result = validate(single_provider(azure_entry(Some("2025-03-01-preview"))));

    assert_eq!(result, Ok(()));
}

#[test]
fn provider_rejects_invalid_host() {
    for host in ["", "a/b", "${HOST}", "h@x", "a?b", "a#b", "a b"] {
        let result = validate(single_provider(openai_entry_with(json!({ "host": host }))));

        assert!(result.is_err(), "host {host:?} must be rejected");
    }
}

#[test]
fn provider_accepts_valid_host() {
    for host in [
        "api.openai.com",
        "localhost:8080",
        "[::1]",
        "my_host-1.internal",
    ] {
        let result = validate(single_provider(openai_entry_with(json!({ "host": host }))));

        assert_eq!(result, Ok(()), "host {host:?} must be accepted");
    }
}

#[test]
fn provider_rejects_port_zero() {
    let result = validate(single_provider(openai_entry_with(json!({ "port": 0 }))));

    assert!(result.is_err());
}

#[test]
fn provider_accepts_explicit_port() {
    let result = validate(single_provider(openai_entry_with(
        json!({ "port": 8443, "use_http": true }),
    )));

    assert_eq!(result, Ok(()));
}

#[test]
fn provider_rag_provider_must_reference_existing_entry() {
    let result = validate(json!({
        "providers": {
            "openai": openai_entry(),
            "anthropic": {
                "kind": "anthropic_messages",
                "host": "api.anthropic.com",
                "storage_kind": "openai",
                "rag_provider": "missing"
            }
        }
    }));

    assert!(result.is_err());
}

#[test]
fn provider_rag_provider_referencing_existing_entry_is_accepted() {
    let result = validate(json!({
        "providers": {
            "openai": openai_entry(),
            "anthropic": {
                "kind": "anthropic_messages",
                "host": "api.anthropic.com",
                "storage_kind": "openai",
                "rag_provider": "openai"
            }
        }
    }));

    assert_eq!(result, Ok(()));
}

#[test]
fn tenant_override_without_host_or_alias_is_rejected() {
    for override_entry in [
        json!({}),
        json!({ "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1" }),
        json!({ "auth_config": { "secret_ref": "tenant-key" } }),
    ] {
        let result = validate(tenant_override(&override_entry));

        assert!(
            result.is_err(),
            "override {override_entry} must be rejected"
        );
    }
}

#[test]
fn tenant_override_with_host_or_alias_is_accepted() {
    for override_entry in [
        json!({ "host": "tenant-a.openai.azure.com" }),
        json!({ "upstream_alias": "tenant-a-alias" }),
    ] {
        let result = validate(tenant_override(&override_entry));

        assert_eq!(result, Ok(()), "override {override_entry} must be accepted");
    }
}

#[test]
fn tenant_override_rejects_invalid_host() {
    for host in ["a/b", "${TENANT_HOST}", "h@x"] {
        let result = validate(tenant_override(&json!({ "host": host })));

        assert!(result.is_err(), "override host {host:?} must be rejected");
    }
}

// ── Knowledge search ─────────────────────────────────────────────────────

#[test]
fn knowledge_search_enabled_requires_vector_store_and_provider_ids() {
    for knowledge in [
        json!({ "enabled": true }),
        json!({ "enabled": true, "vector_store_id": "vs_1" }),
        json!({ "enabled": true, "provider_id": "openai" }),
    ] {
        let result = validate(json!({ "knowledge_search": knowledge.clone() }));

        assert!(
            result.is_err(),
            "knowledge_search {knowledge} must be rejected"
        );
    }
}

#[test]
fn knowledge_search_enabled_with_ids_is_accepted() {
    let result = validate(json!({
        "knowledge_search": { "enabled": true, "vector_store_id": "vs_1", "provider_id": "openai" }
    }));

    assert_eq!(result, Ok(()));
}

#[test]
fn knowledge_search_disabled_without_ids_is_accepted() {
    let result = validate(json!({ "knowledge_search": { "enabled": false } }));

    assert_eq!(result, Ok(()));
}

#[test]
fn knowledge_search_counts_must_be_positive() {
    for key in ["max_calls_per_message", "top_k", "max_chunk_chars"] {
        let mut knowledge = json!({
            "enabled": true,
            "vector_store_id": "vs_1",
            "provider_id": "openai"
        });
        knowledge
            .as_object_mut()
            .expect("knowledge_search is an object")
            .insert(key.to_owned(), json!(0));

        let result = validate(json!({ "knowledge_search": knowledge }));

        assert!(
            result.is_err(),
            "knowledge_search.{key} = 0 must be rejected"
        );
    }
}

// ── Repository config ────────────────────────────────────────────────────

#[test]
fn yaml_gear_section_deserializes_and_validates() {
    let raw = yaml_gear_section();

    let cfg: MiniChatConfig = serde_json::from_value(raw).expect("yaml section must deserialize");

    assert_eq!(cfg.validate(), Ok(()));
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 10);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 90);
    assert_eq!(cfg.thread_summary_worker.summary_model_id, "gpt-4.1-mini");
    assert_eq!(cfg.providers.len(), 1);
    let azure = cfg.providers.get("azure_openai").expect("azure provider");
    assert_eq!(variant_name(&azure.storage_kind), "azure");
    assert_eq!(azure.api_version.as_deref(), Some("2025-03-01-preview"));
    assert_eq!(azure.api_path, "/openai/v1/responses");
    let auth_config = azure.auth_config.as_ref().expect("auth_config is set");
    assert_eq!(
        auth_config.get("secret_ref").map(String::as_str),
        Some("azure-openai-key")
    );
}

#[test]
fn yaml_gear_section_with_unexpanded_host_fails_validation() {
    let mut raw = yaml_gear_section();
    merge(
        &mut raw,
        json!({ "providers": { "azure_openai": { "host": "${AZURE_OPENAI_API_HOST}" } } }),
    );

    let cfg: MiniChatConfig = serde_json::from_value(raw).expect("yaml section must deserialize");

    assert!(cfg.validate().is_err());
}
