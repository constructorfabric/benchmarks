use super::*;

fn base() -> MiniChatConfig {
    let mut c = MiniChatConfig {
        client_credentials: Some(ClientCredentialsConfig {
            client_id: "mini-chat".into(),
            client_secret: "secret".into(),
        }),
        ..MiniChatConfig::default()
    };
    c.expand_and_normalize();
    c
}

type Mutator = Box<dyn Fn(&mut MiniChatConfig)>;

fn from_json(v: serde_json::Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(v)
}

#[test]
#[allow(clippy::cognitive_complexity, reason = "flat list of default-value assertions")]
fn defaults_are_valid_and_match_appendix_b() {
    let c = base();
    c.validate().unwrap();
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
    assert_eq!(c.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert_eq!(c.thumbnail.max_bytes, 131_072);
    assert_eq!(c.orphan_watchdog.timeout_secs, 300);
    assert_eq!(c.upload_reaper.stale_after_secs, 300);
    assert_eq!(c.thread_summary_worker.effective_summary_model_id(), "gpt-4.1-mini");
    assert_eq!(c.cleanup_worker.max_attempts, 5);
    let openai = &c.providers["openai"];
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.upstream_alias.as_deref(), Some("api.openai.com"));
    assert_eq!(openai.effective_port(), 443);
}

#[test]
fn unknown_top_level_key_is_rejected() {
    let r = from_json(serde_json::json!({"client_credentials": {"client_id":"a","client_secret":"b"}, "mcp": {}}));
    assert!(r.is_err());
}

#[test]
fn unknown_streaming_key_is_rejected() {
    let r = from_json(serde_json::json!({"streaming": {"web_search_context_size": "low"}}));
    assert!(r.is_err(), "removed key must fail");
}

#[test]
fn removed_provider_key_is_rejected() {
    let r = from_json(serde_json::json!({"providers": {"p": {"kind":"openai_responses","host":"h","storage_kind":"openai","supports_file_search_filters": true}}}));
    assert!(r.is_err());
}

#[test]
fn worker_sections_accept_unknown_keys() {
    let r = from_json(serde_json::json!({
        "client_credentials": {"client_id":"a","client_secret":"b"},
        "orphan_watchdog": {"scan_interval_secs": 10, "timeout_secs": 90, "something": 1},
        "thread_summary_worker": {"whatever": true}
    }));
    let c = r.unwrap();
    assert_eq!(c.orphan_watchdog.scan_interval_secs, 10);
}

#[test]
fn missing_client_credentials_fails_validation() {
    let mut c = MiniChatConfig::default();
    c.expand_and_normalize();
    assert!(c.validate().is_err());
}

#[test]
fn range_checks() {
    let cases: Vec<(&str, Mutator)> = vec![
        ("ping low", Box::new(|c| c.streaming.sse_ping_interval_seconds = 4)),
        ("ping high", Box::new(|c| c.streaming.sse_ping_interval_seconds = 61)),
        ("channel", Box::new(|c| c.streaming.sse_channel_capacity = 8)),
        ("floor zero", Box::new(|c| c.estimation_budgets.minimal_generation_floor = 0)),
        ("floor above max", Box::new(|c| c.estimation_budgets.minimal_generation_floor = 40_000)),
        ("overshoot", Box::new(|c| c.quota.overshoot_tolerance_factor = 1.6)),
        ("warning", Box::new(|c| c.quota.warning_threshold_pct = 100)),
        ("partitions", Box::new(|c| c.outbox.num_partitions = 3)),
        ("recent", Box::new(|c| c.context.recent_messages_limit = 101)),
        ("watchdog timeout", Box::new(|c| c.orphan_watchdog.timeout_secs = 89)),
        ("reaper stale", Box::new(|c| c.upload_reaper.stale_after_secs = 59)),
        ("claim", Box::new(|c| c.thread_summary_worker.claim_timeout_secs = 29)),
        ("concurrency", Box::new(|c| c.rag.max_concurrent_uploads = 0)),
    ];
    for (name, mutate) in cases {
        let mut c = base();
        mutate(&mut c);
        assert!(c.validate().is_err(), "{name} must fail validation");
    }
}

#[test]
fn azure_requires_api_version() {
    let mut c = base();
    let mut p = c.providers["openai"].clone();
    p.storage_kind = Some(StorageKind::Azure);
    p.api_version = None;
    c.providers.insert("azure".into(), p.clone());
    assert!(c.validate().is_err());
    p.api_version = Some("2025-03-01-preview".into());
    c.providers.insert("azure".into(), p);
    c.validate().unwrap();
}

#[test]
fn rag_provider_must_exist() {
    let mut c = base();
    let mut p = c.providers["openai"].clone();
    p.rag_provider = Some("missing".into());
    c.providers.insert("x".into(), p);
    assert!(c.validate().is_err());
}

#[test]
fn host_character_check_after_expansion() {
    let mut c = base();
    let mut p = c.providers["openai"].clone();
    p.host = "evil.com/path".into();
    c.providers.insert("x".into(), p);
    assert!(c.validate().is_err());
}

#[test]
fn env_expansion() {
    // SAFETY-free: use a variable that is certainly undefined.
    assert_eq!(expand_env("a${MINI_CHAT_SURELY_UNDEFINED_VAR}b"), "ab");
    assert_eq!(expand_env("plain"), "plain");
    assert_eq!(expand_env("x${UNTERMINATED"), "x${UNTERMINATED");
}

#[test]
fn alias_defaults_to_host() {
    let c = from_json(serde_json::json!({
        "client_credentials": {"client_id":"a","client_secret":"b"},
        "providers": {"p": {"kind":"openai_responses","host":"127.0.0.1","port":8000,"use_http":true,"storage_kind":"openai",
            "tenant_overrides": {"t1": {"host": "10.0.0.1"}}}}
    }));
    let mut c = c.unwrap();
    c.expand_and_normalize();
    c.validate().unwrap();
    let p = &c.providers["p"];
    assert_eq!(p.upstream_alias.as_deref(), Some("127.0.0.1"));
    assert_eq!(p.tenant_overrides["t1"].upstream_alias.as_deref(), Some("10.0.0.1"));
    assert_eq!(p.effective_port(), 8000);
}

#[test]
fn knowledge_search_requires_ids_when_enabled() {
    let mut c = base();
    c.knowledge_search.enabled = true;
    assert!(c.validate().is_err());
    c.knowledge_search.vector_store_id = Some("vs_1".into());
    c.knowledge_search.provider_id = Some("openai".into());
    c.validate().unwrap();
}
