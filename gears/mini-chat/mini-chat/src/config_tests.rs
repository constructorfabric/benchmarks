#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;

use super::{MiniChatConfig, ProviderKind, StorageKind};

fn base() -> serde_json::Value {
    json!({ "client_credentials": { "client_id": "mini-chat", "client_secret": "s" } })
}

#[test]
#[allow(clippy::cognitive_complexity)] // reason: flat list of default-value assertions
fn defaults_follow_appendix_b() {
    let cfg = MiniChatConfig::from_value(Some(&base())).unwrap();
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
    assert_eq!(cfg.streaming.max_output_tokens, 32768);
    assert_eq!(cfg.estimation_budgets.minimal_generation_floor, 50);
    assert!((cfg.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(cfg.quota.warning_threshold_pct, 80);
    assert_eq!(cfg.quota.web_search_max_calls_per_message, 2);
    assert_eq!(cfg.quota.web_search_daily_quota, 75);
    assert_eq!(cfg.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(cfg.quota.code_interpreter_daily_quota, 50);
    assert_eq!(cfg.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(cfg.outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(cfg.outbox.num_partitions, 4);
    assert_eq!(cfg.context.recent_messages_limit, 10);
    assert_eq!(cfg.rag.max_documents_per_chat, 50);
    assert_eq!(cfg.rag.max_total_upload_mb_per_chat, 100);
    assert_eq!(cfg.rag.uploaded_file_max_size_kb, 25600);
    assert_eq!(cfg.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(cfg.rag.max_images_per_message, 4);
    assert_eq!(cfg.thumbnail.max_bytes, 131_072);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 300);
    assert_eq!(cfg.thread_summary_worker.max_attempts, 3);
    assert_eq!(cfg.thread_summary_worker.summary_model(), "gpt-4.1-mini");
    assert_eq!(cfg.cleanup_worker.max_attempts, 5);
    let openai = cfg.providers.get("openai").unwrap();
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.storage_kind, Some(StorageKind::Openai));
    assert_eq!(openai.upstream_alias.as_deref(), Some("api.openai.com"));
}

#[test]
fn rejects_unknown_top_level_and_section_keys() {
    let mut v = base();
    v["bogus"] = json!(1);
    assert!(MiniChatConfig::from_value(Some(&v)).is_err());
    let mut v = base();
    v["streaming"] = json!({ "web_search_context_size": "low" });
    assert!(MiniChatConfig::from_value(Some(&v)).is_err());
    let mut v = base();
    v["providers"] = json!({ "p": { "kind": "openai_responses", "host": "h", "storage_kind": "openai", "supports_file_search_filters": true } });
    assert!(MiniChatConfig::from_value(Some(&v)).is_err());
}

#[test]
fn worker_sections_accept_unknown_keys() {
    let mut v = base();
    v["orphan_watchdog"] = json!({ "scan_interval_secs": 10, "timeout_secs": 90, "whatever": 1 });
    v["cleanup_worker"] = json!({ "foo": "bar" });
    let cfg = MiniChatConfig::from_value(Some(&v)).unwrap();
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 10);
}

#[test]
fn range_checks() {
    for (section, key, value) in [
        ("streaming", "sse_ping_interval_seconds", json!(4)),
        ("streaming", "sse_channel_capacity", json!(65)),
        ("quota", "overshoot_tolerance_factor", json!(1.6)),
        ("quota", "warning_threshold_pct", json!(100)),
        ("outbox", "num_partitions", json!(3)),
        ("orphan_watchdog", "timeout_secs", json!(89)),
        ("upload_reaper", "stale_after_secs", json!(59)),
        ("thread_summary_worker", "claim_timeout_secs", json!(10)),
        ("estimation_budgets", "minimal_generation_floor", json!(0)),
        ("context", "recent_messages_limit", json!(101)),
    ] {
        let mut v = base();
        v[section] = json!({ key: value });
        assert!(MiniChatConfig::from_value(Some(&v)).is_err(), "{section}.{key}");
    }
}

#[test]
fn client_credentials_required() {
    assert!(MiniChatConfig::from_value(Some(&json!({}))).is_err());
}

#[test]
fn azure_requires_api_version_and_rag_provider_must_exist() {
    let mut v = base();
    v["providers"] = json!({ "az": { "kind": "openai_responses", "host": "h", "storage_kind": "azure" } });
    assert!(MiniChatConfig::from_value(Some(&v)).is_err());
    v["providers"]["az"]["api_version"] = json!("2025-03-01-preview");
    assert!(MiniChatConfig::from_value(Some(&v)).is_ok());
    v["providers"]["an"] = json!({ "kind": "anthropic_messages", "host": "a", "rag_provider": "missing" });
    assert!(MiniChatConfig::from_value(Some(&v)).is_err());
    v["providers"]["an"]["rag_provider"] = json!("az");
    assert!(MiniChatConfig::from_value(Some(&v)).is_ok());
}

#[test]
fn host_character_check_and_alias_default() {
    let mut v = base();
    v["providers"] = json!({ "p": { "kind": "openai_responses", "host": "evil/path", "storage_kind": "openai" } });
    assert!(MiniChatConfig::from_value(Some(&v)).is_err());
    v["providers"] = json!({ "p": { "kind": "openai_responses", "host": "127.0.0.1", "port": 9999, "use_http": true, "storage_kind": "openai",
        "tenant_overrides": { "t1": { "host": "10.0.0.1" } } } });
    let cfg = MiniChatConfig::from_value(Some(&v)).unwrap();
    let p = cfg.providers.get("p").unwrap();
    assert_eq!(p.alias(), "127.0.0.1");
    assert_eq!(p.effective_port(), 9999);
    assert_eq!(p.tenant_overrides["t1"].upstream_alias.as_deref(), Some("10.0.0.1"));
}

#[test]
fn env_var_expansion_in_host_and_auth_config() {
    temp_env::with_var("MC_TEST_HOST", Some("example.org"), || {
        let mut v = base();
        v["providers"] = json!({ "p": { "kind": "openai_responses", "host": "${MC_TEST_HOST}", "storage_kind": "openai",
            "auth_config": { "secret_ref": "${MC_TEST_MISSING:-fallback}" } } });
        let cfg = MiniChatConfig::from_value(Some(&v)).unwrap();
        assert_eq!(cfg.providers["p"].host, "example.org");
        assert_eq!(cfg.providers["p"].auth_config["secret_ref"], "fallback");
    });
}

#[test]
fn deprecated_fields_are_reported() {
    let mut v = base();
    v["cleanup_worker"] = json!({ "enabled": false, "batch_size": 7 });
    v["estimation_budgets"] = json!({ "safety_margin_pct": 30 });
    let cfg = MiniChatConfig::from_value(Some(&v)).unwrap();
    let w = cfg.deprecated_field_warnings();
    assert!(w.contains(&"cleanup_worker.enabled"));
    assert!(w.contains(&"cleanup_worker.batch_size"));
    assert!(w.contains(&"estimation_budgets.safety_margin_pct"));
}
