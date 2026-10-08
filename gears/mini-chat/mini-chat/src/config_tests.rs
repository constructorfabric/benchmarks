use super::*;

fn with_creds(mut v: serde_json::Value) -> serde_json::Value {
    v["client_credentials"] = serde_json::json!({"client_id": "a", "client_secret": "b"});
    v
}

#[test]
fn defaults_match_appendix_b() {
    let c: MiniChatConfig = serde_json::from_value(with_creds(serde_json::json!({}))).unwrap();
    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.vendor, "constructorfabric");
    assert_eq!(c.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(c.streaming.sse_channel_capacity, 32);
    assert_eq!(c.streaming.max_output_tokens, 32768);
    assert_eq!(c.estimation_budgets.minimal_generation_floor, 50);
    assert_eq!(c.quota.warning_threshold_pct, 80);
    assert_eq!(c.quota.web_search_daily_quota, 75);
    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(c.outbox.num_partitions, 4);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert_eq!(c.rag.uploaded_file_max_size_kb, 25600);
    assert_eq!(c.thumbnail.max_bytes, 131_072);
    assert_eq!(c.orphan_watchdog.timeout_secs, 300);
    assert_eq!(c.upload_reaper.stale_after_secs, 300);
    assert_eq!(c.thread_summary_worker.summary_model(), "gpt-4.1-mini");
    assert_eq!(c.cleanup_worker.max_attempts, 5);
    assert!(c.providers.contains_key("openai"));
    c.validate().unwrap();
}

#[test]
fn unknown_keys_rejected_in_strict_sections() {
    assert!(serde_json::from_value::<MiniChatConfig>(with_creds(serde_json::json!({"mcp": {}}))).is_err());
    assert!(
        serde_json::from_value::<MiniChatConfig>(with_creds(
            serde_json::json!({"streaming": {"web_search_context_size": "low"}})
        ))
        .is_err()
    );
}

#[test]
fn worker_sections_accept_unknown_keys() {
    let c: MiniChatConfig = serde_json::from_value(with_creds(
        serde_json::json!({"orphan_watchdog": {"scan_interval_secs": 10, "timeout_secs": 90, "foo": 1}}),
    ))
    .unwrap();
    assert_eq!(c.orphan_watchdog.scan_interval_secs, 10);
    c.validate().unwrap();
}

#[test]
fn validation_ranges() {
    let mut c: MiniChatConfig = serde_json::from_value(with_creds(serde_json::json!({}))).unwrap();
    c.streaming.sse_ping_interval_seconds = 4;
    assert!(c.validate().is_err());
    c.streaming.sse_ping_interval_seconds = 15;
    c.orphan_watchdog.timeout_secs = 89;
    assert!(c.validate().is_err());
    c.orphan_watchdog.timeout_secs = 300;
    c.estimation_budgets.minimal_generation_floor = 0;
    assert!(c.validate().is_err());
    c.estimation_budgets.minimal_generation_floor = 50;
    c.outbox.num_partitions = 3;
    assert!(c.validate().is_err());
    c.outbox.num_partitions = 4;
    c.client_credentials.client_id = String::new();
    assert!(c.validate().is_err());
}

#[test]
fn azure_requires_api_version() {
    let c: MiniChatConfig = serde_json::from_value(with_creds(serde_json::json!({
        "providers": {"az": {"kind": "openai_responses", "host": "x.openai.azure.com", "storage_kind": "azure"}}
    })))
    .unwrap();
    assert!(c.validate().is_err());
}

#[test]
fn removed_provider_field_rejected() {
    assert!(
        serde_json::from_value::<MiniChatConfig>(with_creds(serde_json::json!({
            "providers": {"o": {"kind": "openai_responses", "host": "h", "storage_kind": "openai",
                                 "supports_file_search_filters": true}}
        })))
        .is_err()
    );
}
