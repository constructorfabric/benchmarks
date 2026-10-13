use super::*;

fn valid() -> MiniChatConfig {
    let mut c = MiniChatConfig::default();
    c.client_credentials.client_id = "mini-chat".into();
    c.client_credentials.client_secret = "secret".into();
    c.expand_and_normalize().unwrap();
    c
}

#[test]
fn defaults_match_appendix_b() {
    let c = MiniChatConfig::default();
    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(c.streaming.sse_channel_capacity, 32);
    assert_eq!(c.streaming.max_output_tokens, 32768);
    assert_eq!(c.estimation_budgets.minimal_generation_floor, 50);
    assert!((c.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(c.quota.warning_threshold_pct, 80);
    assert_eq!(c.quota.web_search_max_calls_per_message, 2);
    assert_eq!(c.quota.web_search_daily_quota, 75);
    assert_eq!(c.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(c.quota.code_interpreter_daily_quota, 50);
    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(c.outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(c.outbox.num_partitions, 4);
    assert_eq!(c.context.recent_messages_limit, 10);
    assert_eq!(c.rag.max_documents_per_chat, 50);
    assert_eq!(c.rag.max_total_upload_mb_per_chat, 100);
    assert_eq!(c.rag.uploaded_file_max_size_kb, 25600);
    assert_eq!(c.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert_eq!(c.thumbnail.width, 128);
    assert_eq!(c.thumbnail.max_bytes, 131_072);
    assert_eq!(c.orphan_watchdog.timeout_secs, 300);
    assert_eq!(c.upload_reaper.stale_after_secs, 300);
    assert_eq!(c.thread_summary_worker.summary_model(), "gpt-4.1-mini");
    assert_eq!(c.cleanup_worker.max_attempts, 5);
}

#[test]
fn valid_config_passes() {
    assert!(valid().validate().is_ok());
}

#[test]
fn rejects_out_of_range_values() {
    let mut c = valid();
    c.streaming.sse_ping_interval_seconds = 4;
    assert!(c.validate().is_err());
    let mut c = valid();
    c.streaming.sse_channel_capacity = 65;
    assert!(c.validate().is_err());
    let mut c = valid();
    c.quota.overshoot_tolerance_factor = 1.6;
    assert!(c.validate().is_err());
    let mut c = valid();
    c.orphan_watchdog.timeout_secs = 89;
    assert!(c.validate().is_err());
    let mut c = valid();
    c.outbox.num_partitions = 3;
    assert!(c.validate().is_err());
    let mut c = valid();
    c.estimation_budgets.minimal_generation_floor = 0;
    assert!(c.validate().is_err());
    let mut c = valid();
    c.client_credentials.client_id = String::new();
    assert!(c.validate().is_err());
}

#[test]
fn removed_and_unknown_keys_fail() {
    let err = serde_json::from_value::<MiniChatConfig>(serde_json::json!({
        "streaming": { "web_search_context_size": "low" }
    }));
    assert!(err.is_err());
    let err = serde_json::from_value::<MiniChatConfig>(serde_json::json!({ "mcp": {} }));
    assert!(err.is_err());
    let ok = serde_json::from_value::<MiniChatConfig>(serde_json::json!({
        "orphan_watchdog": { "something_unknown": 1 }
    }));
    assert!(ok.is_ok());
}

#[test]
fn azure_requires_api_version_and_alias_defaults_to_host() {
    let mut c = valid();
    let mut p = c.providers.get("openai").unwrap().clone();
    p.storage_kind = Some(StorageKind::Azure);
    p.api_version = None;
    c.providers.insert("az".into(), p);
    assert!(c.validate().is_err());
    let c = valid();
    assert_eq!(c.providers["openai"].upstream_alias.as_deref(), Some("api.openai.com"));
}

#[test]
fn deprecated_fields_are_reported() {
    let mut c = valid();
    c.cleanup_worker.batch_size = 1;
    assert_eq!(c.deprecated_fields_in_use(), vec!["cleanup_worker.batch_size"]);
}
