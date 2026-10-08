use super::*;

fn valid() -> MiniChatConfig {
    let mut c = MiniChatConfig::default();
    c.client_credentials.client_id = "mini-chat".to_owned();
    c.client_credentials.client_secret = "secret".to_owned();
    c
}

#[test]
#[allow(clippy::cognitive_complexity)]
fn defaults_match_design_appendix_b() {
    let c = MiniChatConfig::default();
    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.vendor, "constructorfabric");
    assert_eq!(c.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(c.streaming.sse_channel_capacity, 32);
    assert_eq!(c.streaming.max_output_tokens, 32_768);
    assert_eq!(c.estimation_budgets.minimal_generation_floor, 50);
    assert_eq!(c.quota.warning_threshold_pct, 80);
    assert_eq!(c.quota.web_search_max_calls_per_message, 2);
    assert_eq!(c.quota.web_search_daily_quota, 75);
    assert_eq!(c.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(c.quota.code_interpreter_daily_quota, 50);
    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(c.outbox.num_partitions, 4);
    assert_eq!(c.context.recent_messages_limit, 10);
    assert_eq!(c.rag.uploaded_file_max_size_kb, 25_600);
    assert_eq!(c.rag.uploaded_image_max_size_kb, 5_120);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert_eq!(c.rag.max_documents_per_chat, 50);
    assert_eq!(c.rag.max_total_upload_mb_per_chat, 100);
    assert_eq!(c.rag.max_concurrent_uploads, 10);
    assert_eq!(c.thumbnail.max_bytes, 131_072);
    assert_eq!(c.orphan_watchdog.timeout_secs, 300);
    assert_eq!(c.upload_reaper.stale_after_secs, 300);
    assert_eq!(c.thread_summary_worker.compression_threshold_pct, 80);
    assert_eq!(c.thread_summary_worker.effective_summary_model_id(), "gpt-4.1-mini");
    assert_eq!(c.cleanup_worker.max_attempts, 5);
    let openai = c.providers.get("openai").expect("default provider");
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.effective_port(), 443);
}

#[test]
#[allow(clippy::type_complexity)]
fn validation_rejects_out_of_range_values() {
    assert!(valid().validate().is_ok());
    let cases: Vec<(&str, Box<dyn Fn(&mut MiniChatConfig)>)> = vec![
        ("missing creds", Box::new(|c| c.client_credentials.client_id.clear())),
        ("ping", Box::new(|c| c.streaming.sse_ping_interval_seconds = 4)),
        ("channel", Box::new(|c| c.streaming.sse_channel_capacity = 65)),
        ("floor", Box::new(|c| c.estimation_budgets.minimal_generation_floor = 0)),
        ("overshoot", Box::new(|c| c.quota.overshoot_tolerance_factor = 1.6)),
        ("partitions", Box::new(|c| c.outbox.num_partitions = 3)),
        ("watchdog", Box::new(|c| c.orphan_watchdog.timeout_secs = 89)),
        ("reaper", Box::new(|c| c.upload_reaper.stale_after_secs = 59)),
        ("recent", Box::new(|c| c.context.recent_messages_limit = 101)),
        (
            "azure needs api_version",
            Box::new(|c| {
                if let Some(p) = c.providers.get_mut("openai") {
                    p.storage_kind = Some(StorageKind::Azure);
                }
            }),
        ),
        (
            "bad host",
            Box::new(|c| {
                if let Some(p) = c.providers.get_mut("openai") {
                    p.host = "evil/host".to_owned();
                }
            }),
        ),
    ];
    for (name, mutate) in cases {
        let mut c = valid();
        mutate(&mut c);
        assert!(c.validate().is_err(), "case '{name}' should fail validation");
    }
}

#[test]
fn unknown_keys_rejected_in_strict_sections_but_not_workers() {
    let strict = serde_json::json!({"streaming": {"web_search_context_size": "low"}});
    assert!(serde_json::from_value::<MiniChatConfig>(strict).is_err());
    let provider = serde_json::json!({"providers": {"p": {"kind": "openai_responses", "host": "h", "storage_kind": "openai", "supports_file_search_filters": true}}});
    assert!(serde_json::from_value::<MiniChatConfig>(provider).is_err());
    let worker = serde_json::json!({"orphan_watchdog": {"timeout_secs": 120, "whatever": 1}});
    let c: MiniChatConfig = serde_json::from_value(worker).expect("worker sections lenient");
    assert_eq!(c.orphan_watchdog.timeout_secs, 120);
}

#[test]
fn provider_entry_parses_operator_shape() {
    let v = serde_json::json!({
        "providers": {"azure_openai": {
            "kind": "openai_responses", "storage_kind": "azure", "host": "x.openai.azure.com",
            "api_path": "/openai/v1/responses", "api_version": "2025-03-01-preview",
            "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "auth_config": {"header": "api-key", "prefix": "", "secret_ref": "azure-openai-key"}
        }},
        "client_credentials": {"client_id": "a", "client_secret": "b"}
    });
    let c: MiniChatConfig = serde_json::from_value(v).expect("parses");
    assert!(c.validate().is_ok());
    assert_eq!(c.providers.len(), 1, "providers map replaces the default");
}

#[test]
fn deprecated_fields_are_reported() {
    let mut c = valid();
    assert!(c.deprecated_field_warnings().is_empty());
    c.cleanup_worker.batch_size = 1;
    c.estimation_budgets.image_token_budget = 1;
    assert_eq!(c.deprecated_field_warnings().len(), 2);
}
