use super::*;
use serde_json::json;

fn base() -> serde_json::Value {
    json!({"client_credentials": {"client_id": "mini-chat", "client_secret": "s"}})
}

fn parse(v: serde_json::Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(v)
}

#[test]
fn defaults_match_appendix_b() {
    let c = parse(base()).unwrap();
    c.validate().unwrap();
    assert_core_defaults(&c);
    assert_outbox_and_rag_defaults(&c);
    assert_worker_and_provider_defaults(&c);
}

fn assert_core_defaults(c: &MiniChatConfig) {
    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.vendor, "constructorfabric");
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
}

fn assert_outbox_and_rag_defaults(c: &MiniChatConfig) {
    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(c.outbox.cleanup_queue_name, "mini-chat.attachment_cleanup");
    assert_eq!(c.outbox.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(c.outbox.thread_summary_queue_name, "mini-chat.thread_summary");
    assert_eq!(c.outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(c.outbox.num_partitions, 4);
    assert_eq!(c.context.recent_messages_limit, 10);
    assert_eq!(c.rag.uploaded_file_max_size_kb, 25_600);
    assert_eq!(c.rag.uploaded_image_max_size_kb, 5_120);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert_eq!(c.rag.max_documents_per_chat, 50);
    assert_eq!(c.rag.max_total_upload_mb_per_chat, 100);
    assert!(c.rag.allow_csv_upload);
    assert_eq!(c.rag.max_concurrent_uploads, 10);
    assert_eq!(c.thumbnail.width, 128);
    assert_eq!(c.thumbnail.max_bytes, 131_072);
}

fn assert_worker_and_provider_defaults(c: &MiniChatConfig) {
    assert!(!c.knowledge_search.enabled);
    assert_eq!(c.orphan_watchdog.timeout_secs, 300);
    assert_eq!(c.orphan_watchdog.scan_interval_secs, 60);
    assert_eq!(c.upload_reaper.stale_after_secs, 300);
    assert_eq!(c.thread_summary_worker.claim_timeout_secs, 300);
    assert_eq!(c.thread_summary_worker.max_attempts, 3);
    assert_eq!(c.thread_summary_worker.effective_model_id(), "gpt-4.1-mini");
    assert_eq!(c.cleanup_worker.max_attempts, 5);
    let openai = &c.providers["openai"];
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.host, "api.openai.com");
    assert_eq!(openai.storage_kind, Some(StorageKind::Openai));
    assert_eq!(openai.effective_port(), 443);
    assert!(c.deprecated_fields_in_use().is_empty());
}

#[test]
fn strict_sections_reject_unknown_keys() {
    for (section, body) in [
        ("top", json!({"unknown": 1})),
        ("streaming", json!({"streaming": {"web_search_context_size": "low"}})),
        ("quota", json!({"quota": {"nope": 1}})),
        ("rag", json!({"rag": {"nope": 1}})),
        ("outbox", json!({"outbox": {"nope": 1}})),
        ("context", json!({"context": {"nope": 1}})),
        ("thumbnail", json!({"thumbnail": {"nope": 1}})),
        ("provider", json!({"providers": {"p": {"kind": "openai_responses", "host": "h", "storage_kind": "openai", "supports_file_search_filters": true}}})),
        ("mcp", json!({"mcp": {"enabled": true}})),
    ] {
        let mut v = base();
        for (k, val) in body.as_object().unwrap() {
            v[k] = val.clone();
        }
        assert!(parse(v).is_err(), "{section} must reject unknown keys");
    }
}

#[test]
fn worker_sections_accept_unknown_keys() {
    let mut v = base();
    v["orphan_watchdog"] = json!({"scan_interval_secs": 10, "timeout_secs": 90, "extra": 1});
    v["upload_reaper"] = json!({"whatever": true});
    v["thread_summary_worker"] = json!({"enabled": true, "summary_model_id": "gpt-4.1-mini", "x": 1});
    v["cleanup_worker"] = json!({"batch_size": 7, "y": 1});
    let c = parse(v).unwrap();
    c.validate().unwrap();
    assert_eq!(c.orphan_watchdog.timeout_secs, 90);
    assert_eq!(c.deprecated_fields_in_use(), vec!["cleanup_worker.batch_size"]);
}

#[test]
fn validation_ranges() {
    let cases = [
        json!({"streaming": {"sse_ping_interval_seconds": 4}}),
        json!({"streaming": {"sse_channel_capacity": 65}}),
        json!({"estimation_budgets": {"minimal_generation_floor": 0}}),
        json!({"streaming": {"max_output_tokens": 10}, "estimation_budgets": {"minimal_generation_floor": 11}}),
        json!({"quota": {"overshoot_tolerance_factor": 1.6}}),
        json!({"quota": {"warning_threshold_pct": 100}}),
        json!({"quota": {"web_search_daily_quota": 0}}),
        json!({"outbox": {"num_partitions": 3}}),
        json!({"context": {"recent_messages_limit": 101}}),
        json!({"rag": {"max_concurrent_uploads": 0}}),
        json!({"orphan_watchdog": {"timeout_secs": 89}}),
        json!({"upload_reaper": {"stale_after_secs": 59}}),
        json!({"thread_summary_worker": {"claim_timeout_secs": 29}}),
        json!({"thread_summary_worker": {"compression_threshold_pct": 100}}),
        json!({"knowledge_search": {"enabled": true}}),
        json!({"providers": {"a": {"kind": "openai_responses", "host": "h", "storage_kind": "azure"}}}),
        json!({"providers": {"a": {"kind": "openai_responses", "host": "h/x", "storage_kind": "openai"}}}),
        json!({"providers": {"a": {"kind": "openai_responses", "host": "h", "storage_kind": "openai", "rag_provider": "zzz"}}}),
        json!({"providers": {"a": {"kind": "openai_responses", "host": "h"}}}),
        json!({"providers": {"a": {"kind": "openai_responses", "host": "h", "storage_kind": "openai", "tenant_overrides": {"t": {}}}}}),
    ];
    for case in cases {
        let mut v = base();
        for (k, val) in case.as_object().unwrap() {
            v[k] = val.clone();
        }
        let c = parse(v.clone()).unwrap();
        assert!(c.validate().is_err(), "expected validation error for {case}");
    }
    let missing_creds = parse(json!({})).unwrap();
    assert!(missing_creds.validate().is_err());
}

#[test]
fn provider_entry_full_shape_and_env_expansion() {
    use toolkit::var_expand::ExpandVars;
    // SAFETY-free: test-only env var with a unique name.
    temp_env::with_var("MINI_CHAT_TEST_HOST", Some("expanded.example.com"), || {
        let mut v = base();
        v["providers"] = json!({
            "azure_openai": {
                "kind": "openai_responses",
                "storage_kind": "azure",
                "host": "${MINI_CHAT_TEST_HOST}",
                "api_path": "/openai/v1/responses",
                "api_version": "2025-03-01-preview",
                "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "auth_config": {"header": "api-key", "prefix": "", "secret_ref": "azure-openai-key"},
                "tenant_overrides": {"00000000-0000-0000-0000-000000000001": {"host": "t.example.com"}}
            }
        });
        let mut c = parse(v).unwrap();
        c.expand_vars().unwrap();
        c.validate().unwrap();
        let p = &c.providers["azure_openai"];
        assert_eq!(p.host, "expanded.example.com");
        assert_eq!(p.storage_kind, Some(StorageKind::Azure));
        assert_eq!(p.auth_config.as_ref().unwrap()["header"], "api-key");
    });
}

#[test]
fn credentials_secret_is_redacted_in_debug() {
    let c = parse(base()).unwrap();
    let dbg = format!("{:?}", c.client_credentials);
    assert!(!dbg.contains("\"s\""));
    assert!(dbg.contains("REDACTED"));
}
