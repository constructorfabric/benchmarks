#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cognitive_complexity)]

use super::*;
use serde_json::json;

fn base() -> serde_json::Value {
    json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "s" },
        "providers": { "p": { "kind": "openai_responses", "host": "127.0.0.1", "port": 9000,
            "use_http": true, "storage_kind": "openai" } }
    })
}

#[test]
fn defaults_match_design_appendix_b() {
    let c: MiniChatConfig = serde_json::from_value(base()).unwrap();
    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.vendor, "constructorfabric");
    assert_eq!(c.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(c.streaming.sse_channel_capacity, 32);
    assert_eq!(c.streaming.max_output_tokens, 32768);
    assert_eq!(c.context.recent_messages_limit, 10);
    assert_eq!(c.estimation_budgets.minimal_generation_floor, 50);
    assert!((c.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(c.quota.warning_threshold_pct, 80);
    assert_eq!(c.quota.web_search_max_calls_per_message, 2);
    assert_eq!(c.quota.web_search_daily_quota, 75);
    assert_eq!(c.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(c.quota.code_interpreter_daily_quota, 50);
    assert_eq!(c.rag.max_documents_per_chat, 50);
    assert_eq!(c.rag.max_total_upload_mb_per_chat, 100);
    assert_eq!(c.rag.uploaded_file_max_size_kb, 25600);
    assert_eq!(c.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert_eq!(c.rag.max_concurrent_uploads, 10);
    assert_eq!(c.thumbnail.max_bytes, 131_072);
    assert_eq!(c.orphan_watchdog.timeout_secs, 300);
    assert_eq!(c.upload_reaper.stale_after_secs, 300);
    assert_eq!(c.cleanup_worker.max_attempts, 5);
    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(c.outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(c.outbox.num_partitions, 4);
    assert_eq!(c.thread_summary_worker.compression_threshold_pct, 80);
    assert_eq!(c.thread_summary_worker.effective_model_id(), "gpt-4.1-mini");
    assert_eq!(c.thread_summary_worker.message_content_limit, 4000);
    c.validate().unwrap();
}

#[test]
fn unknown_top_level_key_rejected() {
    let mut v = base();
    v["mcp"] = json!({});
    assert!(serde_json::from_value::<MiniChatConfig>(v).is_err());
}

#[test]
fn unknown_provider_key_rejected() {
    let mut v = base();
    v["providers"]["p"]["supports_file_search_filters"] = json!(true);
    assert!(serde_json::from_value::<MiniChatConfig>(v).is_err());
}

#[test]
fn worker_sections_accept_unknown_keys() {
    let mut v = base();
    v["orphan_watchdog"] = json!({ "scan_interval_secs": 10, "timeout_secs": 90, "whatever": 1 });
    v["thread_summary_worker"] = json!({ "enabled": true, "summary_model_id": "x", "foo": "bar" });
    let c: MiniChatConfig = serde_json::from_value(v).unwrap();
    c.validate().unwrap();
}

#[test]
fn validation_ranges() {
    let mut c: MiniChatConfig = serde_json::from_value(base()).unwrap();
    c.streaming.sse_ping_interval_seconds = 4;
    assert!(c.validate().is_err());
    let mut c: MiniChatConfig = serde_json::from_value(base()).unwrap();
    c.orphan_watchdog.timeout_secs = 89;
    assert!(c.validate().is_err());
    let mut c: MiniChatConfig = serde_json::from_value(base()).unwrap();
    c.outbox.num_partitions = 3;
    assert!(c.validate().is_err());
    let mut c: MiniChatConfig = serde_json::from_value(base()).unwrap();
    c.estimation_budgets.minimal_generation_floor = 0;
    assert!(c.validate().is_err());
    let mut v = base();
    v["providers"]["p"]["storage_kind"] = json!("azure");
    let c: MiniChatConfig = serde_json::from_value(v).unwrap();
    assert!(c.validate().is_err(), "azure requires api_version");
    let mut v = base();
    v["providers"]["p"]["host"] = json!("evil/host");
    let c: MiniChatConfig = serde_json::from_value(v).unwrap();
    assert!(c.validate().is_err());
}

#[test]
fn alias_defaults_to_host() {
    let mut c: MiniChatConfig = serde_json::from_value(base()).unwrap();
    c.fill_aliases();
    assert_eq!(
        c.providers["p"].upstream_alias.as_deref(),
        Some("127.0.0.1")
    );
    assert_eq!(c.providers["p"].effective_api_path(), "/v1/responses");
    assert_eq!(c.providers["p"].effective_port(), 9000);
}

#[test]
fn repo_dev_config_section_parses() {
    let yaml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/mini-chat.yaml"
    ))
    .unwrap();
    let doc: serde_json::Value = serde_saphyr_like(&yaml);
    let cfg = &doc["gears"]["mini-chat"]["config"];
    let c: MiniChatConfig = serde_json::from_value(cfg.clone()).unwrap();
    assert!(c.providers.contains_key("azure_openai"));
}

fn serde_saphyr_like(yaml: &str) -> serde_json::Value {
    serde_saphyr::from_str::<serde_json::Value>(yaml).expect("yaml")
}
