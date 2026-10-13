#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};

use super::{MiniChatConfig, ProviderKind, StorageKind};

fn base() -> Value {
    json!({
        "client_credentials": { "client_id": "id", "client_secret": "secret" },
        "providers": {
            "openai": {
                "kind": "openai_responses",
                "host": "llm.test",
                "storage_kind": "openai"
            }
        }
    })
}

fn with(patch: &Value) -> Value {
    let mut config = base();
    let (Some(target), Some(patch)) = (config.as_object_mut(), patch.as_object()) else {
        panic!("objects expected");
    };
    for (key, value) in patch {
        target.insert(key.clone(), value.clone());
    }
    config
}

fn parse(config: Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(config)
}

fn valid(patch: &Value) -> bool {
    parse(with(patch)).is_ok_and(|cfg| cfg.validate().is_ok())
}

#[test]
fn defaults_follow_appendix_b() {
    let cfg = parse(base()).unwrap();

    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
    assert_eq!(cfg.streaming.max_output_tokens, 32_768);
    assert_eq!(cfg.estimation_budgets.minimal_generation_floor, 50);
    assert!((cfg.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(cfg.quota.warning_threshold_pct, 80);
    assert_eq!(cfg.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(cfg.outbox.num_partitions, 4);
    assert_eq!(cfg.context.recent_messages_limit, 10);
    assert_eq!(cfg.rag.max_concurrent_uploads, 10);
    assert_eq!(cfg.thumbnail.max_bytes, 131_072);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 300);
    assert_eq!(cfg.thread_summary_worker.claim_timeout_secs, 300);
    assert_eq!(cfg.cleanup_worker.max_attempts, 5);
    assert!(cfg.validate().is_ok());
}

#[test]
fn absent_providers_section_defaults_to_the_openai_entry() {
    let cfg = parse(json!({
        "client_credentials": { "client_id": "id", "client_secret": "secret" }
    }))
    .unwrap();

    let openai = &cfg.providers["openai"];
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.host, "api.openai.com");
    assert_eq!(openai.storage_kind, StorageKind::Openai);
    assert_eq!(openai.effective_port(), 443);
    assert!(cfg.validate().is_ok());
}

#[test]
fn client_credentials_are_required() {
    assert!(parse(json!({ "providers": {} })).is_err());
}

#[test]
fn unknown_keys_are_rejected_where_appendix_b_says_so() {
    for patch in [
        json!({ "bogus": 1 }),
        json!({ "streaming": { "bogus": 1 } }),
        json!({ "quota": { "bogus": 1 } }),
        json!({ "outbox": { "bogus": 1 } }),
        json!({ "context": { "bogus": 1 } }),
        json!({ "rag": { "bogus": 1 } }),
        json!({ "metrics": { "bogus": 1 } }),
        json!({ "thumbnail": { "bogus": 1 } }),
        json!({ "knowledge_search": { "bogus": 1 } }),
        json!({ "estimation_budgets": { "bogus": 1 } }),
        json!({ "streaming": { "web_search_context_size": "low" } }),
    ] {
        assert!(parse(with(&patch)).is_err(), "must be rejected: {patch}");
    }
}

#[test]
fn worker_sections_ignore_unknown_keys() {
    for section in [
        "orphan_watchdog",
        "upload_reaper",
        "thread_summary_worker",
        "cleanup_worker",
    ] {
        assert!(valid(&json!({ section: { "bogus": 1 } })), "{section}");
    }
}

#[test]
fn ranges_are_enforced_at_their_boundaries() {
    let accepted = [
        json!({ "streaming": { "sse_ping_interval_seconds": 5 } }),
        json!({ "streaming": { "sse_ping_interval_seconds": 60 } }),
        json!({ "streaming": { "sse_channel_capacity": 16 } }),
        json!({ "quota": { "overshoot_tolerance_factor": 1.0 } }),
        json!({ "quota": { "overshoot_tolerance_factor": 1.5 } }),
        json!({ "outbox": { "num_partitions": 64 } }),
        json!({ "orphan_watchdog": { "timeout_secs": 90 } }),
        json!({ "upload_reaper": { "stale_after_secs": 86_400 } }),
        json!({ "thread_summary_worker": { "claim_timeout_secs": 30 } }),
    ];
    let rejected = [
        json!({ "streaming": { "sse_ping_interval_seconds": 4 } }),
        json!({ "streaming": { "sse_channel_capacity": 65 } }),
        json!({ "quota": { "overshoot_tolerance_factor": 1.51 } }),
        json!({ "quota": { "warning_threshold_pct": 100 } }),
        json!({ "outbox": { "num_partitions": 3 } }),
        json!({ "outbox": { "num_partitions": 128 } }),
        json!({ "orphan_watchdog": { "timeout_secs": 89 } }),
        json!({ "upload_reaper": { "stale_after_secs": 59 } }),
        json!({ "thread_summary_worker": { "compression_threshold_pct": 100 } }),
        json!({ "rag": { "max_concurrent_uploads": 0 } }),
        json!({ "context": { "recent_messages_limit": 101 } }),
        json!({ "cleanup_worker": { "max_attempts": 0 } }),
    ];

    for patch in accepted {
        assert!(valid(&patch), "must be accepted: {patch}");
    }
    for patch in rejected {
        assert!(!valid(&patch), "must be rejected: {patch}");
    }
}

#[test]
fn minimal_generation_floor_is_bounded_by_the_streaming_output_cap() {
    assert!(!valid(
        &json!({ "estimation_budgets": { "minimal_generation_floor": 0 } })
    ));
    assert!(!valid(&json!({
        "streaming": { "max_output_tokens": 100 },
        "estimation_budgets": { "minimal_generation_floor": 101 }
    })));
    assert!(valid(&json!({
        "streaming": { "max_output_tokens": 100 },
        "estimation_budgets": { "minimal_generation_floor": 100 }
    })));
}

#[test]
fn knowledge_search_requires_its_targets_when_enabled() {
    assert!(!valid(&json!({ "knowledge_search": { "enabled": true } })));
    assert!(valid(&json!({
        "knowledge_search": { "enabled": true, "vector_store_id": "vs", "provider_id": "openai" }
    })));
}

#[test]
fn provider_rules_follow_appendix_b() {
    let provider = |patch: Value| {
        let mut entry = base()["providers"]["openai"].clone();
        if let (Some(target), Some(patch)) = (entry.as_object_mut(), patch.as_object()) {
            for (key, value) in patch {
                target.insert(key.clone(), value.clone());
            }
        }
        json!({ "providers": { "openai": entry, "other": { "kind": "vllm_responses", "host": "o.test", "storage_kind": "openai" } } })
    };

    assert!(valid(&provider(json!({}))));
    assert!(!valid(&provider(json!({ "host": "" }))));
    assert!(!valid(&provider(json!({ "host": "a/b" }))));
    assert!(!valid(&provider(json!({ "port": 0 }))));
    assert!(!valid(&provider(json!({ "storage_kind": "azure" }))));
    assert!(!valid(&provider(
        json!({ "storage_kind": "azure", "api_version": "1&x" })
    )));
    assert!(valid(&provider(
        json!({ "storage_kind": "azure", "api_version": "2025-03-01-preview" })
    )));
    assert!(valid(&provider(json!({ "rag_provider": "other" }))));
    assert!(!valid(&provider(json!({ "rag_provider": "missing" }))));
    assert!(
        parse(with(&provider(
            json!({ "supports_file_search_filters": true })
        )))
        .is_err()
    );
}

#[test]
fn tenant_overrides_need_a_tenant_uuid_and_a_host_or_alias() {
    let tenant = uuid::Uuid::new_v4().to_string();
    let overrides = |key: &str, value: Value| {
        json!({ "providers": { "openai": {
            "kind": "openai_responses", "host": "llm.test", "storage_kind": "openai",
            "tenant_overrides": { key: value }
        } } })
    };

    assert!(valid(&overrides(&tenant, json!({ "host": "t.llm.test" }))));
    assert!(valid(&overrides(
        &tenant,
        json!({ "upstream_alias": "t-alias" })
    )));
    assert!(!valid(&overrides(&tenant, json!({}))));
    assert!(!valid(&overrides(
        "not-a-uuid",
        json!({ "host": "t.llm.test" })
    )));
    assert!(!valid(&overrides(&tenant, json!({ "host": "t/x" }))));
    assert!(
        parse(with(&overrides(
            &tenant,
            json!({ "host": "t.llm.test", "bogus": 1 })
        )))
        .is_err()
    );
}

#[test]
fn url_prefix_must_be_a_path_without_trailing_slash() {
    assert!(valid(&json!({ "url_prefix": "/chat-api" })));
    assert!(!valid(&json!({ "url_prefix": "chat-api" })));
    assert!(!valid(&json!({ "url_prefix": "/chat-api/" })));
    assert!(!valid(&json!({ "url_prefix": "/chat{api}" })));
}

#[test]
fn empty_identity_values_are_rejected() {
    assert!(!valid(&json!({ "vendor": "" })));
    assert!(!valid(
        &json!({ "client_credentials": { "client_id": "", "client_secret": "s" } })
    ));
    assert!(!valid(
        &json!({ "client_credentials": { "client_id": "i", "client_secret": "" } })
    ));
    assert!(!valid(&json!({ "outbox": { "queue_name": "" } })));
}

#[test]
fn deprecated_fields_with_non_default_values_produce_warnings() {
    let defaults = parse(base()).unwrap();
    assert!(defaults.deprecation_warnings().is_empty());

    let cfg = parse(with(&json!({
        "estimation_budgets": { "fixed_overhead_tokens": 1 },
        "cleanup_worker": { "enabled": false, "batch_size": 7 },
        "thread_summary_worker": { "reconcile_interval_secs": 5 }
    })))
    .unwrap();

    let warnings = cfg.deprecation_warnings();
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("estimation_budgets.fixed_overhead_tokens"))
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("cleanup_worker.enabled"))
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("cleanup_worker.batch_size"))
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("thread_summary_worker.reconcile_interval_secs"))
    );
}

#[test]
fn summary_model_defaults_to_the_built_in_model() {
    let cfg = parse(base()).unwrap();
    assert_eq!(
        cfg.thread_summary_worker.effective_summary_model_id(),
        "gpt-4.1-mini"
    );

    let cfg = parse(with(
        &json!({ "thread_summary_worker": { "summary_model_id": "m" } }),
    ))
    .unwrap();
    assert_eq!(cfg.thread_summary_worker.effective_summary_model_id(), "m");
}
