use super::*;

fn base_yaml() -> serde_json::Value {
    serde_json::json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "secret" },
        "providers": {
            "openai": {
                "kind": "openai_responses",
                "host": "127.0.0.1",
                "port": 9000,
                "use_http": true,
                "storage_kind": "openai"
            }
        }
    })
}

fn parse(v: serde_json::Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(v)
}

#[test]
#[allow(clippy::cognitive_complexity)]
fn defaults_match_design() {
    let cfg = parse(base_yaml()).unwrap();
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
    assert_eq!(cfg.streaming.max_output_tokens, 32_768);
    assert_eq!(cfg.estimation_budgets.minimal_generation_floor, 50);
    assert!((cfg.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(cfg.quota.warning_threshold_pct, 80);
    assert_eq!(cfg.quota.web_search_max_calls_per_message, 2);
    assert_eq!(cfg.quota.web_search_daily_quota, 75);
    assert_eq!(cfg.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(cfg.quota.code_interpreter_daily_quota, 50);
    assert_eq!(cfg.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(
        cfg.outbox.cleanup_queue_name,
        "mini-chat.attachment_cleanup"
    );
    assert_eq!(cfg.outbox.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(
        cfg.outbox.thread_summary_queue_name,
        "mini-chat.thread_summary"
    );
    assert_eq!(cfg.outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(cfg.outbox.num_partitions, 4);
    assert_eq!(cfg.context.recent_messages_limit, 10);
    assert_eq!(cfg.rag.max_documents_per_chat, 50);
    assert_eq!(cfg.rag.max_total_upload_mb_per_chat, 100);
    assert_eq!(cfg.rag.uploaded_file_max_size_kb, 25_600);
    assert_eq!(cfg.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(cfg.rag.max_images_per_message, 4);
    assert_eq!(cfg.rag.max_concurrent_uploads, 10);
    assert_eq!(cfg.thumbnail.width, 128);
    assert_eq!(cfg.thumbnail.max_bytes, 131_072);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 60);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 300);
    assert_eq!(cfg.thread_summary_worker.compression_threshold_pct, 80);
    assert_eq!(cfg.thread_summary_worker.claim_timeout_secs, 300);
    assert_eq!(cfg.thread_summary_worker.max_attempts, 3);
    assert_eq!(
        cfg.thread_summary_worker.effective_summary_model_id(),
        "gpt-4.1-mini"
    );
    assert_eq!(cfg.cleanup_worker.max_attempts, 5);
    assert!(!cfg.knowledge_search.enabled);
    cfg.validate().unwrap();
}

#[test]
fn unknown_top_level_key_is_rejected() {
    let mut v = base_yaml();
    v["mcp"] = serde_json::json!({ "enabled": true });
    assert!(parse(v).is_err());
}

#[test]
fn removed_provider_key_is_rejected() {
    let mut v = base_yaml();
    v["providers"]["openai"]["supports_file_search_filters"] = serde_json::json!(true);
    assert!(parse(v).is_err());
}

#[test]
fn removed_streaming_key_is_rejected() {
    let mut v = base_yaml();
    v["streaming"] = serde_json::json!({ "web_search_context_size": "low" });
    assert!(parse(v).is_err());
}

#[test]
fn worker_sections_ignore_unknown_keys() {
    let mut v = base_yaml();
    v["orphan_watchdog"] = serde_json::json!({ "timeout_secs": 120, "whatever": 1 });
    v["thread_summary_worker"] = serde_json::json!({ "bogus": true });
    let cfg = parse(v).unwrap();
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 120);
}

#[test]
fn azure_requires_api_version() {
    let mut v = base_yaml();
    v["providers"]["openai"]["storage_kind"] = serde_json::json!("azure");
    let cfg = parse(v).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn ranges_are_validated() {
    for (path, value) in [
        (
            ("streaming", "sse_ping_interval_seconds"),
            serde_json::json!(4),
        ),
        (("streaming", "sse_channel_capacity"), serde_json::json!(65)),
        (
            ("quota", "overshoot_tolerance_factor"),
            serde_json::json!(1.6),
        ),
        (("quota", "warning_threshold_pct"), serde_json::json!(0)),
        (("orphan_watchdog", "timeout_secs"), serde_json::json!(89)),
        (("upload_reaper", "stale_after_secs"), serde_json::json!(59)),
        (("outbox", "num_partitions"), serde_json::json!(3)),
        (
            ("estimation_budgets", "minimal_generation_floor"),
            serde_json::json!(0),
        ),
    ] {
        let mut v = base_yaml();
        v[path.0] = serde_json::json!({ path.1: value });
        let cfg = parse(v).unwrap();
        assert!(
            cfg.validate().is_err(),
            "{}.{} should be rejected",
            path.0,
            path.1
        );
    }
}

#[test]
fn aliases_default_to_host() {
    let mut cfg = parse(base_yaml()).unwrap();
    cfg.fill_default_aliases();
    assert_eq!(
        cfg.providers["openai"].upstream_alias.as_deref(),
        Some("127.0.0.1")
    );
}

#[test]
fn deprecated_fields_warn() {
    let mut v = base_yaml();
    v["cleanup_worker"] = serde_json::json!({ "batch_size": 7 });
    v["estimation_budgets"] = serde_json::json!({ "safety_margin_pct": 50 });
    let cfg = parse(v).unwrap();
    let w = cfg.deprecation_warnings();
    assert_eq!(w.len(), 2, "{w:?}");
}

#[test]
fn rag_provider_must_exist() {
    let mut v = base_yaml();
    v["providers"]["anthropic"] = serde_json::json!({
        "kind": "anthropic_messages",
        "host": "api.anthropic.com",
        "rag_provider": "missing"
    });
    let cfg = parse(v).unwrap();
    assert!(cfg.validate().is_err());
}
