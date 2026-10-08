#![allow(clippy::unwrap_used, clippy::expect_used)]

use secrecy::SecretString;
use serde_json::json;
use uuid::Uuid;

use super::*;

fn valid() -> MiniChatConfig {
    let mut cfg = MiniChatConfig::default();
    cfg.client_credentials.client_id = "mini-chat".to_owned();
    cfg.client_credentials.client_secret = SecretString::from("secret");
    cfg.apply_defaults();
    cfg
}

fn parse(v: serde_json::Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(v)
}

#[test]
fn defaults_are_valid_and_match_appendix_b() {
    let mut cfg = MiniChatConfig::default();
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
    assert_eq!(cfg.outbox.num_partitions, 4);
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
    assert_eq!(cfg.context.recent_messages_limit, 10);
    assert_eq!(cfg.context.web_search_guard, DEFAULT_WEB_SEARCH_GUARD);
    assert_eq!(cfg.rag.max_images_per_message, 4);
    assert_eq!(cfg.rag.max_documents_per_chat, 50);
    assert_eq!(cfg.rag.max_concurrent_uploads, 10);
    assert_eq!(cfg.thumbnail.max_decode_bytes, 33_554_432);
    assert!(!cfg.knowledge_search.enabled);
    assert_eq!(cfg.knowledge_search.top_k, 5);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 300);
    assert_eq!(cfg.thread_summary_worker.max_attempts, 3);
    assert_eq!(cfg.thread_summary_worker.message_content_limit, 4000);
    assert_eq!(cfg.cleanup_worker.max_attempts, 5);
    assert_eq!(cfg.metrics.prefix, "");

    assert_eq!(cfg.providers.len(), 1);
    let openai = &cfg.providers["openai"];
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.storage_kind, StorageKind::Openai);
    assert_eq!(openai.host, "api.openai.com");
    assert_eq!(openai.api_path, "/v1/responses");
    assert_eq!(openai.auth_config["secret_ref"], "cred://openai-key");

    // client_credentials are required.
    assert!(cfg.validate().is_err());
    cfg.client_credentials.client_id = "id".to_owned();
    cfg.client_credentials.client_secret = SecretString::from("s");
    cfg.validate().unwrap();
    assert!(cfg.deprecation_warnings().is_empty());
}

#[test]
fn empty_json_object_equals_default() {
    let cfg = parse(json!({})).unwrap();
    assert_eq!(cfg.providers.len(), 1);
    assert_eq!(cfg.url_prefix, "/mini-chat");
}

#[test]
fn guards_have_documented_text() {
    assert!(DEFAULT_WEB_SEARCH_GUARD.starts_with("Use web_search only if"));
    assert!(DEFAULT_WEB_SEARCH_GUARD.ends_with("At most one web_search call per request."));
    assert!(DEFAULT_SUMMARY_SYSTEM_PROMPT.starts_with("You are a conversation summarizer."));
    let tsw = ThreadSummaryWorkerConfig::default();
    assert_eq!(tsw.summary_system_prompt, DEFAULT_SUMMARY_SYSTEM_PROMPT);
    assert_eq!(tsw.effective_model_id(), "gpt-4.1-mini");
    let custom = ThreadSummaryWorkerConfig {
        summary_model_id: "m".to_owned(),
        ..ThreadSummaryWorkerConfig::default()
    };
    assert_eq!(custom.effective_model_id(), "m");
}

#[test]
fn unknown_top_level_key_rejected() {
    assert!(parse(json!({"nope": 1})).is_err());
    assert!(parse(json!({"mcp": {}})).is_err());
}

#[test]
fn unknown_streaming_key_rejected() {
    assert!(parse(json!({"streaming": {"web_search_context_size": "low"}})).is_err());
    assert!(parse(json!({"quota": {"x": 1}})).is_err());
    assert!(parse(json!({"rag": {"x": 1}})).is_err());
    assert!(parse(json!({"knowledge_search": {"x": 1}})).is_err());
    assert!(parse(json!({"client_credentials": {"x": 1}})).is_err());
}

#[test]
fn removed_provider_key_rejected() {
    let v = json!({"providers": {"p": {
        "kind": "openai_responses", "host": "h", "storage_kind": "openai",
        "supports_file_search_filters": true
    }}});
    assert!(parse(v).is_err());
}

#[test]
fn unknown_key_in_worker_sections_accepted() {
    let cfg = parse(json!({
        "orphan_watchdog": {"foo": 1, "scan_interval_secs": 10},
        "upload_reaper": {"foo": 1},
        "thread_summary_worker": {"foo": 1},
        "cleanup_worker": {"foo": 1}
    }))
    .unwrap();
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 10);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
}

#[test]
fn provider_required_fields_and_enums() {
    // kind, host and storage_kind are required.
    assert!(parse(json!({"providers": {"p": {"host": "h", "storage_kind": "openai"}}})).is_err());
    assert!(
        parse(json!({"providers": {"p": {"kind": "openai_responses", "storage_kind": "openai"}}}))
            .is_err()
    );
    assert!(parse(json!({"providers": {"p": {"kind": "openai_responses", "host": "h"}}})).is_err());
    for kind in [
        "openai_responses",
        "openai_chat_completions",
        "vllm_responses",
        "anthropic_messages",
    ] {
        parse(json!({"providers": {"p": {"kind": kind, "host": "h", "storage_kind": "azure"}}}))
            .unwrap();
    }
    assert!(
        parse(json!({"providers": {"p": {"kind": "bogus", "host": "h", "storage_kind": "azure"}}}))
            .is_err()
    );
}

fn with_mut(f: impl FnOnce(&mut MiniChatConfig)) -> Result<(), String> {
    let mut cfg = valid();
    f(&mut cfg);
    cfg.validate()
}

#[test]
fn range_validation() {
    let cases: Vec<(&str, Box<dyn Fn(&mut MiniChatConfig)>)> = vec![
        (
            "ping 4",
            Box::new(|c| c.streaming.sse_ping_interval_seconds = 4),
        ),
        (
            "ping 61",
            Box::new(|c| c.streaming.sse_ping_interval_seconds = 61),
        ),
        (
            "capacity 15",
            Box::new(|c| c.streaming.sse_channel_capacity = 15),
        ),
        (
            "capacity 65",
            Box::new(|c| c.streaming.sse_channel_capacity = 65),
        ),
        (
            "floor 0",
            Box::new(|c| c.estimation_budgets.minimal_generation_floor = 0),
        ),
        (
            "floor > max_output",
            Box::new(|c| {
                c.estimation_budgets.minimal_generation_floor = c.streaming.max_output_tokens + 1
            }),
        ),
        (
            "overshoot 0.99",
            Box::new(|c| c.quota.overshoot_tolerance_factor = 0.99),
        ),
        (
            "overshoot 1.51",
            Box::new(|c| c.quota.overshoot_tolerance_factor = 1.51),
        ),
        (
            "overshoot NaN",
            Box::new(|c| c.quota.overshoot_tolerance_factor = f64::NAN),
        ),
        ("warning 0", Box::new(|c| c.quota.warning_threshold_pct = 0)),
        (
            "warning 100",
            Box::new(|c| c.quota.warning_threshold_pct = 100),
        ),
        (
            "web max calls 0",
            Box::new(|c| c.quota.web_search_max_calls_per_message = 0),
        ),
        (
            "web daily 0",
            Box::new(|c| c.quota.web_search_daily_quota = 0),
        ),
        (
            "ci max calls 0",
            Box::new(|c| c.quota.code_interpreter_max_calls_per_message = 0),
        ),
        (
            "ci daily 0",
            Box::new(|c| c.quota.code_interpreter_daily_quota = 0),
        ),
        ("partitions 0", Box::new(|c| c.outbox.num_partitions = 0)),
        ("partitions 3", Box::new(|c| c.outbox.num_partitions = 3)),
        (
            "partitions 128",
            Box::new(|c| c.outbox.num_partitions = 128),
        ),
        (
            "empty queue",
            Box::new(|c| c.outbox.audit_queue_name.clear()),
        ),
        (
            "orphan timeout 89",
            Box::new(|c| c.orphan_watchdog.timeout_secs = 89),
        ),
        (
            "orphan timeout 3601",
            Box::new(|c| c.orphan_watchdog.timeout_secs = 3601),
        ),
        (
            "orphan scan 0",
            Box::new(|c| c.orphan_watchdog.scan_interval_secs = 0),
        ),
        (
            "reaper stale 59",
            Box::new(|c| c.upload_reaper.stale_after_secs = 59),
        ),
        (
            "reaper stale 86401",
            Box::new(|c| c.upload_reaper.stale_after_secs = 86401),
        ),
        (
            "reaper scan 3601",
            Box::new(|c| c.upload_reaper.scan_interval_secs = 3601),
        ),
        (
            "recent_messages_limit 101",
            Box::new(|c| c.context.recent_messages_limit = 101),
        ),
        (
            "claim_timeout 29",
            Box::new(|c| c.thread_summary_worker.claim_timeout_secs = 29),
        ),
        (
            "claim_timeout 3601",
            Box::new(|c| c.thread_summary_worker.claim_timeout_secs = 3601),
        ),
        (
            "summary attempts 0",
            Box::new(|c| c.thread_summary_worker.max_attempts = 0),
        ),
        (
            "compression 0",
            Box::new(|c| c.thread_summary_worker.compression_threshold_pct = 0),
        ),
        (
            "compression 100",
            Box::new(|c| c.thread_summary_worker.compression_threshold_pct = 100),
        ),
        (
            "cleanup attempts 0",
            Box::new(|c| c.cleanup_worker.max_attempts = 0),
        ),
        ("uploads 0", Box::new(|c| c.rag.max_concurrent_uploads = 0)),
        (
            "uploads 257",
            Box::new(|c| c.rag.max_concurrent_uploads = 257),
        ),
        ("rag docs 0", Box::new(|c| c.rag.max_documents_per_chat = 0)),
        (
            "rag images 0",
            Box::new(|c| c.rag.max_images_per_message = 0),
        ),
        ("thumb width 0", Box::new(|c| c.thumbnail.width = 0)),
        ("empty vendor", Box::new(|c| c.vendor.clear())),
        ("ks top_k 0", Box::new(|c| c.knowledge_search.top_k = 0)),
    ];
    for (name, mutate) in cases {
        assert!(with_mut(mutate).is_err(), "{name} must be rejected");
    }
    // Boundaries are accepted.
    with_mut(|c| {
        c.streaming.sse_ping_interval_seconds = 5;
        c.streaming.sse_channel_capacity = 64;
        c.quota.overshoot_tolerance_factor = 1.5;
        c.quota.warning_threshold_pct = 99;
        c.outbox.num_partitions = 64;
        c.orphan_watchdog.timeout_secs = 90;
        c.upload_reaper.stale_after_secs = 60;
        c.context.recent_messages_limit = 0;
        c.thread_summary_worker.claim_timeout_secs = 30;
        c.estimation_budgets.minimal_generation_floor = c.streaming.max_output_tokens;
    })
    .unwrap();
}

#[test]
fn azure_requires_valid_api_version() {
    let azure = |api_version: Option<&str>| {
        with_mut(|c| {
            let p = c.providers.get_mut("openai").unwrap();
            p.storage_kind = StorageKind::Azure;
            p.api_version = api_version.map(str::to_owned);
        })
    };
    assert!(azure(None).is_err());
    assert!(azure(Some("")).is_err());
    assert!(azure(Some("  ")).is_err());
    assert!(azure(Some("2025-03-01&x=1")).is_err());
    assert!(azure(Some("a b")).is_err());
    azure(Some("2025-03-01-preview")).unwrap();
    // Not required for openai storage.
    with_mut(|_| {}).unwrap();
}

#[test]
fn host_with_slash_rejected() {
    for bad in ["a/b", "h?x", "h#x", "u@h", "", "h h"] {
        let r = with_mut(|c| c.providers.get_mut("openai").unwrap().host = bad.to_owned());
        assert!(r.is_err(), "host {bad:?} must be rejected");
    }
    for good in ["api.openai.com", "[::1]", "my_host-1.example.com:8443"] {
        with_mut(|c| c.providers.get_mut("openai").unwrap().host = good.to_owned()).unwrap();
    }
    // Same check on tenant override host.
    let r = with_mut(|c| {
        c.providers
            .get_mut("openai")
            .unwrap()
            .tenant_overrides
            .insert(
                Uuid::nil(),
                TenantOverride {
                    host: Some("t/x".to_owned()),
                    ..TenantOverride::default()
                },
            );
    });
    assert!(r.is_err());
}

#[test]
fn tenant_override_must_set_host_or_alias() {
    let r = with_mut(|c| {
        c.providers
            .get_mut("openai")
            .unwrap()
            .tenant_overrides
            .insert(Uuid::nil(), TenantOverride::default());
    });
    assert!(r.is_err());
    with_mut(|c| {
        c.providers
            .get_mut("openai")
            .unwrap()
            .tenant_overrides
            .insert(
                Uuid::nil(),
                TenantOverride {
                    upstream_alias: Some("alias".to_owned()),
                    ..TenantOverride::default()
                },
            );
    })
    .unwrap();
}

#[test]
fn port_zero_rejected() {
    assert!(with_mut(|c| c.providers.get_mut("openai").unwrap().port = Some(0)).is_err());
}

#[test]
fn rag_provider_must_exist() {
    let r = with_mut(|c| {
        c.providers.get_mut("openai").unwrap().rag_provider = Some("missing".to_owned());
    });
    assert!(r.is_err());
    with_mut(|c| {
        c.providers.get_mut("openai").unwrap().rag_provider = Some("openai".to_owned());
    })
    .unwrap();
    let cfg = valid();
    let p = &cfg.providers["openai"];
    assert_eq!(p.rag_provider_id("openai"), "openai");
    let mut p2 = p.clone();
    p2.rag_provider = Some("other".to_owned());
    assert_eq!(p2.rag_provider_id("openai"), "other");
}

#[test]
fn knowledge_search_requires_ids_when_enabled() {
    assert!(with_mut(|c| c.knowledge_search.enabled = true).is_err());
    assert!(
        with_mut(|c| {
            c.knowledge_search.enabled = true;
            c.knowledge_search.vector_store_id = Some("vs".to_owned());
        })
        .is_err()
    );
    with_mut(|c| {
        c.knowledge_search.enabled = true;
        c.knowledge_search.vector_store_id = Some("vs".to_owned());
        c.knowledge_search.provider_id = Some("openai".to_owned());
    })
    .unwrap();
}

#[test]
fn empty_client_credentials_rejected() {
    assert!(MiniChatConfig::default().validate().is_err());
    assert!(with_mut(|c| c.client_credentials.client_id.clear()).is_err());
    assert!(with_mut(|c| c.client_credentials.client_secret = SecretString::from("")).is_err());
}

#[test]
fn client_secret_is_redacted_in_debug() {
    let cfg = valid();
    let dbg = format!("{cfg:?}");
    assert!(!dbg.contains("secret\""), "{dbg}");
    assert!(!format!("{:?}", cfg.client_credentials).contains("secret\""));
}

#[test]
fn apply_defaults_fills_alias_backend_and_port() {
    let mut cfg = parse(json!({"providers": {
        "a": {"kind": "openai_responses", "host": "a.example.com", "storage_kind": "openai"},
        "b": {"kind": "vllm_responses", "host": "b.local", "use_http": true,
              "storage_kind": "openai", "upstream_alias": "keep", "storage_backend": "lbl",
              "port": 8000,
              "tenant_overrides": {
                  "00000000-0000-0000-0000-000000000001": {"host": "t.example.com"},
                  "00000000-0000-0000-0000-000000000002": {"host": "t2", "upstream_alias": "t2alias"}
              }},
        "c": {"kind": "vllm_responses", "host": "c.local", "use_http": true, "storage_kind": "openai"}
    }}))
    .unwrap();
    cfg.apply_defaults();
    let a = &cfg.providers["a"];
    assert_eq!(a.upstream_alias.as_deref(), Some("a.example.com"));
    assert_eq!(a.storage_backend.as_deref(), Some("a"));
    assert_eq!(a.port, Some(443));
    let b = &cfg.providers["b"];
    assert_eq!(b.upstream_alias.as_deref(), Some("keep"));
    assert_eq!(b.storage_backend.as_deref(), Some("lbl"));
    assert_eq!(b.port, Some(8000));
    let o1 = &b.tenant_overrides[&Uuid::from_u128(1)];
    assert_eq!(o1.upstream_alias.as_deref(), Some("t.example.com"));
    let o2 = &b.tenant_overrides[&Uuid::from_u128(2)];
    assert_eq!(o2.upstream_alias.as_deref(), Some("t2alias"));
    let c = &cfg.providers["c"];
    assert_eq!(c.port, Some(80));
    assert_eq!(c.effective_port(), 80);
    assert_eq!(c.effective_upstream_alias(), "c.local");
}

#[test]
fn deprecated_cleanup_fields_warn_when_non_default() {
    assert!(valid().deprecation_warnings().is_empty());

    let mut cfg = valid();
    cfg.cleanup_worker.max_attempts = 9; // not deprecated
    assert!(cfg.deprecation_warnings().is_empty());

    cfg.cleanup_worker.enabled = false;
    cfg.cleanup_worker.poll_interval_secs = 1;
    cfg.cleanup_worker.reconcile_interval_secs = 1;
    cfg.cleanup_worker.stale_in_progress_timeout_secs = 1;
    cfg.cleanup_worker.batch_size = 1;
    cfg.thread_summary_worker.reconcile_interval_secs = 1;
    cfg.estimation_budgets.safety_margin_pct = 11;
    cfg.estimation_budgets.image_token_budget = 1;
    let warnings = cfg.deprecation_warnings();
    assert_eq!(warnings.len(), 8, "{warnings:?}");
    for key in [
        "cleanup_worker.enabled",
        "cleanup_worker.poll_interval_secs",
        "cleanup_worker.reconcile_interval_secs",
        "cleanup_worker.stale_in_progress_timeout_secs",
        "cleanup_worker.batch_size",
        "thread_summary_worker.reconcile_interval_secs",
        "estimation_budgets.safety_margin_pct",
        "estimation_budgets.image_token_budget",
    ] {
        assert!(warnings.iter().any(|w| w.contains(key)), "missing {key}");
    }
}

#[test]
fn env_expansion_covers_hosts_auth_overrides_and_credentials() {
    use toolkit::var_expand::ExpandVars as _;
    let tenant = "00000000-0000-0000-0000-0000000000aa";
    let mut cfg = parse(json!({
        "client_credentials": {"client_id": "${MC_T_ID}", "client_secret": "${MC_T_SECRET}"},
        "providers": {"az": {
            "kind": "openai_responses", "storage_kind": "azure", "api_version": "v1",
            "host": "${MC_T_HOST}",
            "auth_config": {"header": "api-key", "secret_ref": "${MC_T_REF}"},
            "tenant_overrides": {tenant: {
                "host": "${MC_T_THOST}",
                "auth_config": {"secret_ref": "${MC_T_TREF}"}
            }}
        }}
    }))
    .unwrap();
    temp_env::with_vars(
        [
            ("MC_T_ID", Some("id1")),
            ("MC_T_SECRET", Some("sec1")),
            ("MC_T_HOST", Some("h.example.com")),
            ("MC_T_REF", Some("ref1")),
            ("MC_T_THOST", Some("t.example.com")),
            ("MC_T_TREF", Some("ref2")),
        ],
        || cfg.expand_vars().unwrap(),
    );
    use secrecy::ExposeSecret as _;
    assert_eq!(cfg.client_credentials.client_id, "id1");
    assert_eq!(cfg.client_credentials.client_secret.expose_secret(), "sec1");
    let az = &cfg.providers["az"];
    assert_eq!(az.host, "h.example.com");
    assert_eq!(az.auth_config["secret_ref"], "ref1");
    let ov = &az.tenant_overrides[&Uuid::parse_str(tenant).unwrap()];
    assert_eq!(ov.host.as_deref(), Some("t.example.com"));
    assert_eq!(ov.auth_config.as_ref().unwrap()["secret_ref"], "ref2");
    cfg.apply_defaults();
    cfg.validate().unwrap();
}

#[test]
fn env_expansion_unset_variable_is_an_error() {
    use toolkit::var_expand::ExpandVars as _;
    let mut cfg = parse(json!({"providers": {"p": {
        "kind": "openai_responses", "storage_kind": "openai", "host": "${MC_T_UNSET_HOST}"
    }}}))
    .unwrap();
    temp_env::with_vars([("MC_T_UNSET_HOST", None::<&str>)], || {
        let err = cfg.expand_vars().unwrap_err().to_string();
        assert!(err.contains("MC_T_UNSET_HOST"), "{err}");
    });
}

#[test]
fn parses_repo_dev_config_section() {
    use toolkit::var_expand::ExpandVars as _;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/mini-chat.yaml"
    );
    let yaml = std::fs::read_to_string(path).unwrap();
    let doc: serde_json::Value = serde_saphyr::from_str(&yaml).unwrap();
    let section = doc["gears"]["mini-chat"]["config"].clone();
    let mut cfg: MiniChatConfig = serde_json::from_value(section).unwrap();
    temp_env::with_vars(
        [("AZURE_OPENAI_API_HOST", Some("res.openai.azure.com"))],
        || {
            cfg.expand_vars().unwrap();
        },
    );
    cfg.apply_defaults();
    cfg.validate().unwrap();
    let az = &cfg.providers["azure_openai"];
    assert_eq!(az.host, "res.openai.azure.com");
    assert_eq!(az.storage_kind, StorageKind::Azure);
    assert_eq!(az.api_path, "/openai/v1/responses");
    assert_eq!(az.upstream_alias.as_deref(), Some("res.openai.azure.com"));
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 90);
    assert_eq!(
        cfg.thread_summary_worker.effective_model_id(),
        "gpt-4.1-mini"
    );
}
