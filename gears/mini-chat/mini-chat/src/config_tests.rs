#![allow(clippy::unwrap_used, clippy::expect_used)]

use secrecy::ExposeSecret;
use serde_json::{Value, json};
use toolkit::var_expand::ExpandVars;

use super::*;

const WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

fn parse(v: Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(v)
}

/// Config with valid client credentials; `patch` keys replace top-level sections.
fn cfg_with(patch: &Value) -> MiniChatConfig {
    let mut base = json!({"client_credentials": {"client_id": "id", "client_secret": "secret"}});
    if let (Some(b), Some(p)) = (base.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            b.insert(k.clone(), v.clone());
        }
    }
    parse(base).expect("config parses")
}

fn invalid(patch: &Value) -> String {
    let cfg = cfg_with(patch);
    cfg.validate().expect_err("config must be rejected")
}

#[test]
fn defaults_match_design() {
    let cfg = parse(json!({})).unwrap();
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.metrics.prefix, "");

    assert_eq!(cfg.providers.len(), 1);
    let openai = &cfg.providers["openai"];
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.host, "api.openai.com");
    assert_eq!(openai.api_path, "/v1/responses");
    assert_eq!(openai.storage_kind, Some(StorageKind::Openai));
    assert_eq!(
        openai.auth_plugin_type.as_deref(),
        Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
    );
    assert_eq!(openai.auth_config["header"], "Authorization");
    assert_eq!(openai.auth_config["prefix"], "Bearer ");
    assert_eq!(openai.auth_config["secret_ref"], "cred://openai-key");
}

#[test]
fn streaming_and_estimation_defaults() {
    let cfg = parse(json!({})).unwrap();
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
    assert_eq!(cfg.streaming.max_output_tokens, 32768);

    let eb = &cfg.estimation_budgets;
    assert_eq!(eb.bytes_per_token_conservative, 4);
    assert_eq!(eb.fixed_overhead_tokens, 100);
    assert_eq!(eb.safety_margin_pct, 10);
    assert_eq!(eb.image_token_budget, 1000);
    assert_eq!(eb.tool_surcharge_tokens, 500);
    assert_eq!(eb.web_search_surcharge_tokens, 500);
    assert_eq!(eb.code_interpreter_surcharge_tokens, 1000);
    assert_eq!(eb.minimal_generation_floor, 50);

    assert!((cfg.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(cfg.quota.warning_threshold_pct, 80);
    assert_eq!(cfg.quota.web_search_max_calls_per_message, 2);
    assert_eq!(cfg.quota.web_search_daily_quota, 75);
    assert_eq!(cfg.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(cfg.quota.code_interpreter_daily_quota, 50);
}

#[test]
fn outbox_and_context_defaults() {
    let cfg = parse(json!({})).unwrap();
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
    assert_eq!(cfg.context.web_search_guard, WEB_SEARCH_GUARD);
    assert!(!cfg.context.file_search_guard.trim().is_empty());
}

#[test]
fn rag_and_thumbnail_defaults() {
    let cfg = parse(json!({})).unwrap();
    assert_eq!(cfg.rag.max_documents_per_chat, 50);
    assert_eq!(cfg.rag.max_total_upload_mb_per_chat, 100);
    assert!(cfg.rag.allow_csv_upload);
    assert_eq!(cfg.rag.max_concurrent_uploads, 10);
    assert_eq!(cfg.rag.uploaded_file_max_size_kb, 25600);
    assert_eq!(cfg.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(cfg.rag.max_images_per_message, 4);

    assert_eq!(cfg.thumbnail.width, 128);
    assert_eq!(cfg.thumbnail.height, 128);
    assert_eq!(cfg.thumbnail.max_bytes, 131_072);
    assert_eq!(cfg.thumbnail.max_pixels, 100_000_000);
    assert_eq!(cfg.thumbnail.max_decode_bytes, 33_554_432);
}

#[test]
fn worker_defaults() {
    let cfg = parse(json!({})).unwrap();
    assert!(cfg.orphan_watchdog.enabled);
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 60);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);

    assert!(cfg.upload_reaper.enabled);
    assert_eq!(cfg.upload_reaper.scan_interval_secs, 60);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 300);

    let ts = &cfg.thread_summary_worker;
    assert!(ts.enabled);
    assert_eq!(ts.claim_timeout_secs, 300);
    assert_eq!(ts.max_attempts, 3);
    assert_eq!(ts.compression_threshold_pct, 80);
    assert_eq!(ts.summary_model_id, "");
    assert_eq!(ts.effective_summary_model_id(), "gpt-4.1-mini");
    assert!(
        ts.summary_system_prompt
            .starts_with("You are a conversation summarizer.")
    );
    assert!(
        ts.summary_system_prompt
            .ends_with("Do not invent information not present in the conversation.")
    );
    assert_eq!(ts.message_content_limit, 4000);
    assert_eq!(ts.reconcile_interval_secs, 60);
}

#[test]
fn cleanup_worker_defaults() {
    let cfg = parse(json!({})).unwrap();
    let cw = &cfg.cleanup_worker;
    assert_eq!(cw.max_attempts, 5);
    assert!(cw.enabled);
    assert_eq!(cw.poll_interval_secs, 60);
    assert_eq!(cw.reconcile_interval_secs, 300);
    assert_eq!(cw.stale_in_progress_timeout_secs, 900);
    assert_eq!(cw.batch_size, 32);
}

#[test]
fn knowledge_search_defaults() {
    let cfg = parse(json!({})).unwrap();
    let ks = &cfg.knowledge_search;
    assert!(!ks.enabled);
    assert!(ks.vector_store_id.is_none());
    assert!(ks.provider_id.is_none());
    assert_eq!(ks.max_calls_per_message, 3);
    assert_eq!(ks.top_k, 5);
    assert_eq!(ks.max_chunk_chars, 2000);
    assert!(!ks.guard.trim().is_empty());
}

#[test]
fn effective_summary_model_id_prefers_configured_value() {
    let cfg = cfg_with(&json!({"thread_summary_worker": {"summary_model_id": "gpt-4.1"}}));
    assert_eq!(
        cfg.thread_summary_worker.effective_summary_model_id(),
        "gpt-4.1"
    );
}

#[test]
fn dev_config_parses() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/mini-chat.yaml"
    );
    let raw = std::fs::read_to_string(path).expect("read config/mini-chat.yaml");
    let doc: Value = serde_saphyr::from_str(&raw).expect("parse yaml");
    let subtree = doc["gears"]["mini-chat"]["config"].clone();
    assert!(subtree.is_object(), "gears.mini-chat.config must exist");

    temp_env::with_var(
        "AZURE_OPENAI_API_HOST",
        Some("example.openai.azure.com"),
        || {
            let mut cfg: MiniChatConfig = serde_json::from_value(subtree).expect("deserialize");
            cfg.expand_vars().expect("expand vars");
            cfg.validate().expect("dev config validates");
            let azure = &cfg.providers["azure_openai"];
            assert_eq!(azure.host, "example.openai.azure.com");
            assert_eq!(azure.storage_kind, Some(StorageKind::Azure));
            assert_eq!(cfg.client_credentials.client_id, "mini-chat");
            assert_eq!(
                cfg.client_credentials.client_secret.expose_secret(),
                "mini-chat-dev-secret"
            );
            assert_eq!(cfg.orphan_watchdog.timeout_secs, 90);
        },
    );
}

#[test]
fn client_credentials_expand_env_vars() {
    temp_env::with_vars(
        [
            ("MC_TEST_CLIENT_ID", Some("cid")),
            ("MC_TEST_CLIENT_SECRET", Some("sec")),
        ],
        || {
            let mut cfg = parse(json!({"client_credentials": {
                "client_id": "${MC_TEST_CLIENT_ID}", "client_secret": "${MC_TEST_CLIENT_SECRET}"
            }}))
            .unwrap();
            cfg.expand_vars().unwrap();
            assert_eq!(cfg.client_credentials.client_id, "cid");
            assert_eq!(cfg.client_credentials.client_secret.expose_secret(), "sec");
        },
    );
}

#[test]
fn client_credentials_required_non_empty() {
    let cfg = parse(json!({})).unwrap();
    assert!(cfg.validate().unwrap_err().contains("client_credentials"));
    assert!(
        invalid(&json!({"client_credentials": {"client_id": "", "client_secret": "s"}}))
            .contains("client_id")
    );
    assert!(
        invalid(&json!({"client_credentials": {"client_id": "i", "client_secret": " "}}))
            .contains("client_secret")
    );
}

#[test]
fn client_secret_is_redacted_in_debug() {
    let cfg = parse(json!({"client_credentials": {"client_id": "id", "client_secret": "hunter2"}}))
        .unwrap();
    assert!(!format!("{cfg:?}").contains("hunter2"));
}

#[test]
fn valid_minimal_config_passes() {
    cfg_with(&json!({})).validate().unwrap();
}

#[test]
fn url_prefix_must_be_a_leading_slash_path_without_trailing_slash() {
    for bad in ["", "/", "mini-chat", "/mini-chat/", "mini-chat/"] {
        assert!(
            invalid(&json!({"url_prefix": bad})).contains("url_prefix"),
            "{bad:?}"
        );
    }
    for good in ["/mini-chat", "/a/b", "/v1/chat"] {
        cfg_with(&json!({"url_prefix": good})).validate().unwrap();
    }
}

#[test]
fn empty_vendor_rejected() {
    assert!(invalid(&json!({"vendor": ""})).contains("vendor"));
}

#[test]
fn unknown_top_level_key_rejected() {
    let err = parse(json!({"mcp": {}})).unwrap_err().to_string();
    assert!(err.contains("mcp"), "{err}");
}

#[test]
fn removed_keys_rejected() {
    let err = parse(json!({"providers": {"x": {
        "kind": "openai_responses", "host": "h", "storage_kind": "openai",
        "supports_file_search_filters": true
    }}}))
    .unwrap_err()
    .to_string();
    assert!(err.contains("supports_file_search_filters"), "{err}");

    let err = parse(json!({"streaming": {"web_search_context_size": "low"}}))
        .unwrap_err()
        .to_string();
    assert!(err.contains("web_search_context_size"), "{err}");
}

#[test]
fn strict_sections_reject_unknown_keys() {
    for section in [
        "streaming",
        "estimation_budgets",
        "quota",
        "outbox",
        "context",
        "rag",
        "client_credentials",
        "metrics",
        "thumbnail",
        "knowledge_search",
    ] {
        let err = parse(json!({ section: {"bogus_key": 1} }));
        assert!(err.is_err(), "section {section} must reject unknown keys");
    }
    let err = parse(json!({"providers": {"x": {
        "kind": "openai_responses", "host": "h",
        "tenant_overrides": {"t": {"host": "h2", "bogus": 1}}
    }}}));
    assert!(err.is_err(), "tenant override must reject unknown keys");
}

#[test]
fn worker_sections_accept_unknown_keys() {
    let cfg = parse(json!({
        "orphan_watchdog": {"timeout_secs": 100, "future_key": true},
        "upload_reaper": {"future_key": 1},
        "thread_summary_worker": {"future_key": "x"},
        "cleanup_worker": {"future_key": []},
    }))
    .unwrap();
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 100);
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 60);
}

#[test]
fn ping_interval_out_of_range_rejected() {
    for bad in [4, 61] {
        assert!(
            invalid(&json!({"streaming": {"sse_ping_interval_seconds": bad}}))
                .contains("sse_ping_interval_seconds")
        );
    }
    for ok in [5, 60] {
        cfg_with(&json!({"streaming": {"sse_ping_interval_seconds": ok}}))
            .validate()
            .unwrap();
    }
}

#[test]
fn channel_capacity_range() {
    for bad in [15, 65] {
        assert!(
            invalid(&json!({"streaming": {"sse_channel_capacity": bad}}))
                .contains("sse_channel_capacity")
        );
    }
    for ok in [16, 64] {
        cfg_with(&json!({"streaming": {"sse_channel_capacity": ok}}))
            .validate()
            .unwrap();
    }
}

#[test]
fn orphan_timeout_min_90() {
    assert!(
        invalid(&json!({"orphan_watchdog": {"timeout_secs": 89}}))
            .contains("orphan_watchdog.timeout_secs")
    );
    assert!(
        invalid(&json!({"orphan_watchdog": {"timeout_secs": 3601}}))
            .contains("orphan_watchdog.timeout_secs")
    );
    cfg_with(&json!({"orphan_watchdog": {"timeout_secs": 90}}))
        .validate()
        .unwrap();
    assert!(
        invalid(&json!({"orphan_watchdog": {"scan_interval_secs": 0}}))
            .contains("scan_interval_secs")
    );
}

#[test]
fn upload_reaper_and_summary_worker_ranges() {
    assert!(
        invalid(&json!({"upload_reaper": {"stale_after_secs": 59}})).contains("stale_after_secs")
    );
    assert!(
        invalid(&json!({"upload_reaper": {"scan_interval_secs": 3601}}))
            .contains("scan_interval_secs")
    );
    assert!(
        invalid(&json!({"thread_summary_worker": {"claim_timeout_secs": 29}}))
            .contains("claim_timeout_secs")
    );
    assert!(
        invalid(&json!({"thread_summary_worker": {"max_attempts": 0}})).contains("max_attempts")
    );
    assert!(
        invalid(&json!({"thread_summary_worker": {"compression_threshold_pct": 100}}))
            .contains("compression_threshold_pct")
    );
    assert!(invalid(&json!({"cleanup_worker": {"max_attempts": 0}})).contains("max_attempts"));
}

#[test]
fn overshoot_range() {
    for bad in [0.99, 1.51] {
        assert!(
            invalid(&json!({"quota": {"overshoot_tolerance_factor": bad}}))
                .contains("overshoot_tolerance_factor")
        );
    }
    for ok in [1.0, 1.5] {
        cfg_with(&json!({"quota": {"overshoot_tolerance_factor": ok}}))
            .validate()
            .unwrap();
    }
}

#[test]
fn quota_thresholds_and_counts_validated() {
    assert!(
        invalid(&json!({"quota": {"warning_threshold_pct": 0}})).contains("warning_threshold_pct")
    );
    assert!(
        invalid(&json!({"quota": {"warning_threshold_pct": 100}}))
            .contains("warning_threshold_pct")
    );
    for key in [
        "web_search_max_calls_per_message",
        "web_search_daily_quota",
        "code_interpreter_max_calls_per_message",
        "code_interpreter_daily_quota",
    ] {
        assert!(invalid(&json!({"quota": {key: 0}})).contains(key), "{key}");
    }
}

#[test]
fn minimal_generation_floor_bounds() {
    assert!(
        invalid(&json!({"estimation_budgets": {"minimal_generation_floor": 0}}))
            .contains("minimal_generation_floor")
    );
    assert!(
        invalid(&json!({
            "streaming": {"max_output_tokens": 100},
            "estimation_budgets": {"minimal_generation_floor": 101}
        }))
        .contains("minimal_generation_floor")
    );
    cfg_with(&json!({
        "streaming": {"max_output_tokens": 100},
        "estimation_budgets": {"minimal_generation_floor": 100}
    }))
    .validate()
    .unwrap();
}

#[test]
fn deprecated_estimation_fields_are_not_validated_but_warned() {
    let cfg = cfg_with(&json!({"estimation_budgets": {
        "bytes_per_token_conservative": 0, "safety_margin_pct": 99999
    }}));
    cfg.validate().unwrap();
    let warnings = cfg.estimation_budgets.deprecated_warnings();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("bytes_per_token_conservative"))
    );
    assert!(warnings.iter().any(|w| w.contains("safety_margin_pct")));

    let defaults = cfg_with(&json!({}));
    assert!(defaults.estimation_budgets.deprecated_warnings().is_empty());
    // minimal_generation_floor is in use, never deprecated.
    let floor = cfg_with(&json!({"estimation_budgets": {"minimal_generation_floor": 7}}));
    assert!(floor.estimation_budgets.deprecated_warnings().is_empty());
}

#[test]
fn partitions_power_of_two() {
    assert!(invalid(&json!({"outbox": {"num_partitions": 3}})).contains("num_partitions"));
    assert!(invalid(&json!({"outbox": {"num_partitions": 128}})).contains("num_partitions"));
    assert!(invalid(&json!({"outbox": {"num_partitions": 0}})).contains("num_partitions"));
    for ok in [1, 2, 64] {
        cfg_with(&json!({"outbox": {"num_partitions": ok}}))
            .validate()
            .unwrap();
    }
}

#[test]
fn outbox_queue_names_non_empty() {
    for key in [
        "queue_name",
        "cleanup_queue_name",
        "chat_cleanup_queue_name",
        "thread_summary_queue_name",
        "audit_queue_name",
    ] {
        assert!(
            invalid(&json!({"outbox": {key: ""}})).contains(key),
            "{key}"
        );
    }
}

#[test]
fn context_and_rag_limits_validated() {
    assert!(
        invalid(&json!({"context": {"recent_messages_limit": 101}}))
            .contains("recent_messages_limit")
    );
    cfg_with(&json!({"context": {"recent_messages_limit": 0}}))
        .validate()
        .unwrap();
    cfg_with(&json!({"context": {"recent_messages_limit": 100}}))
        .validate()
        .unwrap();
    assert!(
        invalid(&json!({"rag": {"max_concurrent_uploads": 0}})).contains("max_concurrent_uploads")
    );
    assert!(
        invalid(&json!({"rag": {"max_concurrent_uploads": 257}}))
            .contains("max_concurrent_uploads")
    );
    for key in [
        "max_documents_per_chat",
        "max_total_upload_mb_per_chat",
        "uploaded_file_max_size_kb",
        "uploaded_image_max_size_kb",
        "max_images_per_message",
    ] {
        assert!(invalid(&json!({"rag": {key: 0}})).contains(key), "{key}");
    }
    for key in [
        "width",
        "height",
        "max_bytes",
        "max_pixels",
        "max_decode_bytes",
    ] {
        assert!(
            invalid(&json!({"thumbnail": {key: 0}})).contains(key),
            "{key}"
        );
    }
}

#[test]
fn knowledge_search_requires_fields_when_enabled() {
    cfg_with(&json!({"knowledge_search": {"enabled": false}}))
        .validate()
        .unwrap();
    assert!(
        invalid(&json!({"knowledge_search": {"enabled": true, "provider_id": "openai"}}))
            .contains("vector_store_id")
    );
    assert!(
        invalid(&json!({"knowledge_search": {"enabled": true, "vector_store_id": "vs"}}))
            .contains("provider_id")
    );
    for key in ["max_calls_per_message", "top_k", "max_chunk_chars"] {
        assert!(
            invalid(&json!({"knowledge_search": {
                "enabled": true, "vector_store_id": "vs", "provider_id": "openai", key: 0
            }}))
            .contains(key),
            "{key}"
        );
    }
    cfg_with(&json!({"knowledge_search": {
        "enabled": true, "vector_store_id": "vs", "provider_id": "openai"
    }}))
    .validate()
    .unwrap();
}

#[test]
fn azure_requires_api_version() {
    let azure = |extra: Value| {
        let mut p = json!({"kind": "openai_responses", "host": "x.openai.azure.com", "storage_kind": "azure"});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"providers": {"az": p}})
    };
    assert!(invalid(&azure(json!({}))).contains("api_version"));
    assert!(invalid(&azure(json!({"api_version": "  "}))).contains("api_version"));
    assert!(invalid(&azure(json!({"api_version": "2025&x=1"}))).contains("api_version"));
    cfg_with(&azure(json!({"api_version": "2025-03-01-preview"})))
        .validate()
        .unwrap();
}

#[test]
fn storage_kind_required_unless_anthropic_with_rag_provider() {
    assert!(
        invalid(&json!({"providers": {"a": {"kind": "openai_responses", "host": "h"}}}))
            .contains("storage_kind")
    );
    assert!(
        invalid(&json!({"providers": {"a": {"kind": "anthropic_messages", "host": "h"}}}))
            .contains("storage_kind")
    );
    cfg_with(&json!({"providers": {
        "oa": {"kind": "openai_responses", "host": "h", "storage_kind": "openai"},
        "an": {"kind": "anthropic_messages", "host": "h2", "rag_provider": "oa"}
    }}))
    .validate()
    .unwrap();
}

#[test]
fn rag_provider_must_exist() {
    let err = invalid(&json!({"providers": {"an": {
        "kind": "anthropic_messages", "host": "h", "rag_provider": "missing"
    }}}));
    assert!(
        err.contains("rag_provider") && err.contains("missing"),
        "{err}"
    );
}

#[test]
fn host_charset_rejects_slash() {
    for bad in ["a/b", "a?b", "a#b", "u@h", "", "a b"] {
        let err = invalid(&json!({"providers": {"x": {
            "kind": "openai_responses", "host": bad, "storage_kind": "openai"
        }}}));
        assert!(err.contains("host"), "{bad}: {err}");
    }
    for ok in ["api.openai.com", "my_host-1:8443", "[::1]"] {
        cfg_with(&json!({"providers": {"x": {
            "kind": "openai_responses", "host": ok, "storage_kind": "openai"
        }}}))
        .validate()
        .unwrap();
    }
}

#[test]
fn port_zero_rejected_and_effective_port() {
    assert!(
        invalid(&json!({"providers": {"x": {
            "kind": "openai_responses", "host": "h", "storage_kind": "openai", "port": 0
        }}}))
        .contains("port")
    );
    let cfg = cfg_with(&json!({"providers": {
        "a": {"kind": "openai_responses", "host": "h", "storage_kind": "openai"},
        "b": {"kind": "openai_responses", "host": "h", "storage_kind": "openai", "use_http": true},
        "c": {"kind": "openai_responses", "host": "h", "storage_kind": "openai", "port": 8443}
    }}));
    assert_eq!(cfg.providers["a"].effective_port(), 443);
    assert_eq!(cfg.providers["b"].effective_port(), 80);
    assert_eq!(cfg.providers["c"].effective_port(), 8443);
}

#[test]
fn tenant_override_rules() {
    let with = |ov: Value| {
        json!({"providers": {"x": {
            "kind": "openai_responses", "host": "h", "storage_kind": "openai",
            "tenant_overrides": {"t1": ov}
        }}})
    };
    assert!(invalid(&with(json!({"auth_plugin_type": "p"}))).contains("tenant_overrides"));
    assert!(invalid(&with(json!({"host": "a/b"}))).contains("host"));
    cfg_with(&with(json!({"host": "tenant.example.com"})))
        .validate()
        .unwrap();
    cfg_with(&with(json!({"upstream_alias": "alias"})))
        .validate()
        .unwrap();
}

#[test]
fn tenant_override_host_expands_env() {
    temp_env::with_var("MC_TEST_TENANT_HOST", Some("t.example.com"), || {
        let mut cfg = cfg_with(&json!({"providers": {"x": {
            "kind": "openai_responses", "host": "h", "storage_kind": "openai",
            "tenant_overrides": {"t1": {
                "host": "${MC_TEST_TENANT_HOST}",
                "auth_config": {"secret_ref": "${MC_TEST_TENANT_HOST}"}
            }}
        }}}));
        cfg.expand_vars().unwrap();
        let ov = &cfg.providers["x"].tenant_overrides["t1"];
        assert_eq!(ov.host.as_deref(), Some("t.example.com"));
        assert_eq!(
            ov.auth_config.as_ref().unwrap()["secret_ref"],
            "t.example.com"
        );
    });
}

#[test]
fn fill_upstream_aliases_defaults_to_host() {
    let mut cfg = cfg_with(&json!({"providers": {
        "a": {"kind": "openai_responses", "host": "a.example.com", "storage_kind": "openai",
              "tenant_overrides": {
                  "t1": {"host": "t1.example.com"},
                  "t2": {"upstream_alias": "kept"},
                  "t3": {"host": "t3.example.com", "upstream_alias": "kept3"}
              }},
        "b": {"kind": "openai_responses", "host": "b.example.com", "storage_kind": "openai",
              "upstream_alias": "custom"}
    }}));
    cfg.fill_upstream_aliases();
    let a = &cfg.providers["a"];
    assert_eq!(a.upstream_alias.as_deref(), Some("a.example.com"));
    assert_eq!(
        a.tenant_overrides["t1"].upstream_alias.as_deref(),
        Some("t1.example.com")
    );
    assert_eq!(
        a.tenant_overrides["t2"].upstream_alias.as_deref(),
        Some("kept")
    );
    assert_eq!(
        a.tenant_overrides["t3"].upstream_alias.as_deref(),
        Some("kept3")
    );
    assert_eq!(cfg.providers["b"].upstream_alias.as_deref(), Some("custom"));
}

#[test]
fn fill_upstream_aliases_follows_oagw_derived_alias_rule() {
    // OAGW derives `host:port` for a hostname on a non-standard port (scheme-aware:
    // 80 for http, 443 otherwise) and rejects any other explicit alias; IP hosts
    // have no derived alias, so the host itself is passed explicitly.
    let mut cfg = cfg_with(&json!({"providers": {
        "h": {"kind": "openai_responses", "host": "llm.example.com", "port": 8443,
              "storage_kind": "openai",
              "tenant_overrides": {"t1": {"host": "t1.example.com"}}},
        "h80": {"kind": "openai_responses", "host": "plain.example.com", "use_http": true,
                "storage_kind": "openai"},
        "h443http": {"kind": "openai_responses", "host": "odd.example.com", "port": 443,
                     "use_http": true, "storage_kind": "openai"},
        "ip": {"kind": "openai_responses", "host": "127.0.0.1", "port": 9000, "use_http": true,
               "storage_kind": "openai"}
    }}));
    cfg.fill_upstream_aliases();
    let alias = |id: &str| cfg.providers[id].upstream_alias.clone().unwrap();
    assert_eq!(alias("h"), "llm.example.com:8443");
    assert_eq!(
        cfg.providers["h"].tenant_overrides["t1"]
            .upstream_alias
            .as_deref(),
        Some("t1.example.com:8443")
    );
    assert_eq!(alias("h80"), "plain.example.com");
    assert_eq!(alias("h443http"), "odd.example.com:443");
    assert_eq!(alias("ip"), "127.0.0.1");
}

#[test]
fn storage_backend_label_defaults_to_provider_id() {
    let cfg = cfg_with(&json!({"providers": {
        "a": {"kind": "openai_responses", "host": "h", "storage_kind": "openai"},
        "b": {"kind": "openai_responses", "host": "h", "storage_kind": "openai",
              "storage_backend": "custom-label"}
    }}));
    assert_eq!(cfg.providers["a"].storage_backend_label("a"), "a");
    assert_eq!(
        cfg.providers["b"].storage_backend_label("b"),
        "custom-label"
    );
}

#[test]
fn config_deprecated_warnings_cover_every_deprecated_field() {
    assert!(cfg_with(&json!({})).deprecated_warnings().is_empty());
    // max_attempts / enabled=true / the summary worker's live fields are not deprecated.
    let live = cfg_with(&json!({
        "cleanup_worker": {"max_attempts": 9, "enabled": true},
        "thread_summary_worker": {"claim_timeout_secs": 600}
    }));
    assert!(live.deprecated_warnings().is_empty());

    let cfg = cfg_with(&json!({
        "estimation_budgets": {"fixed_overhead_tokens": 1},
        "cleanup_worker": {
            "enabled": false, "poll_interval_secs": 1, "reconcile_interval_secs": 2,
            "stale_in_progress_timeout_secs": 3, "batch_size": 4
        },
        "thread_summary_worker": {"reconcile_interval_secs": 5}
    }));
    let warnings = cfg.deprecated_warnings();
    assert_eq!(warnings.len(), 7, "{warnings:?}");
    for field in [
        "estimation_budgets.fixed_overhead_tokens",
        "cleanup_worker.enabled",
        "cleanup_worker.poll_interval_secs",
        "cleanup_worker.reconcile_interval_secs",
        "cleanup_worker.stale_in_progress_timeout_secs",
        "cleanup_worker.batch_size",
        "thread_summary_worker.reconcile_interval_secs",
    ] {
        assert!(
            warnings.iter().any(|w| w.starts_with(field)),
            "missing warning for {field}: {warnings:?}"
        );
    }
}
