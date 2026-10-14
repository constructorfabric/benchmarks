//! Configuration tests: defaults, ranges, unknown and removed keys, provider rules,
//! deprecated-field warnings and `${VAR}` expansion (DESIGN Appendix B, ADR-0010).
//!
//! Slice S1, algorithm `config-validation` and definition of done `configuration`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]

use secrecy::ExposeSecret;
use serde_json::{Value, json};
use toolkit::var_expand::ExpandVars;

use crate::config::{MiniChatConfig, ProviderKind, StorageKind};

// ── helpers ──────────────────────────────────────────────────────────────────

fn minimal() -> Value {
    json!({ "client_credentials": { "client_id": "mini-chat", "client_secret": "s3cret-value" } })
}

/// Set `path` (dot separated) in `root`, creating objects on the way.
fn set(root: &mut Value, path: &str, value: Value) {
    let mut node = root;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            node[part] = value;
            return;
        }
        if !node[part].is_object() {
            node[part] = json!({});
        }
        node = &mut node[part];
    }
}

fn with(path: &str, value: Value) -> Value {
    let mut raw = minimal();
    set(&mut raw, path, value);
    raw
}

fn ok(raw: Value) -> MiniChatConfig {
    MiniChatConfig::from_value(raw).expect("config must be accepted")
}

fn err(raw: Value) -> String {
    MiniChatConfig::from_value(raw)
        .expect_err("config must be rejected")
        .to_string()
}

fn provider(extra: &Value) -> Value {
    let mut entry =
        json!({ "kind": "openai_responses", "host": "api.openai.com", "storage_kind": "openai" });
    if let Some(extra) = extra.as_object() {
        for (key, value) in extra {
            entry[key] = value.clone();
        }
    }
    entry
}

fn with_provider(extra: &Value) -> Value {
    with("providers.main", provider(extra))
}

// ── defaults (B.1, B.4 to B.9) ───────────────────────────────────────────────

#[test]
fn top_level_defaults_apply() {
    let cfg = ok(minimal());
    assert_eq!(cfg.url_prefix, "/mini-chat");
    assert_eq!(cfg.vendor, "constructorfabric");
    assert_eq!(cfg.metrics.prefix, "");
    assert_eq!(cfg.metrics.effective_prefix(), "mini_chat");
    assert_eq!(cfg.client_credentials.client_id, "mini-chat");
    assert_eq!(
        cfg.client_credentials.client_secret.expose_secret(),
        "s3cret-value"
    );
}

#[test]
fn url_prefix_vendor_and_metrics_prefix_overrides_are_honored() {
    let mut raw = minimal();
    set(&mut raw, "url_prefix", json!("/chat-api"));
    set(&mut raw, "vendor", json!("acme"));
    set(&mut raw, "metrics.prefix", json!("acme_chat"));
    let cfg = ok(raw);
    assert_eq!(cfg.url_prefix, "/chat-api");
    assert_eq!(cfg.vendor, "acme");
    assert_eq!(cfg.metrics.effective_prefix(), "acme_chat");
}

#[test]
fn streaming_defaults_apply() {
    let cfg = ok(minimal());
    assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(cfg.streaming.sse_channel_capacity, 32);
    assert_eq!(cfg.streaming.max_output_tokens, 32_768);
}

#[test]
fn estimation_budget_defaults_apply() {
    let b = ok(minimal()).estimation_budgets;
    assert_eq!(b.minimal_generation_floor, 50);
    assert_eq!(b.bytes_per_token_conservative, 4);
    assert_eq!(b.fixed_overhead_tokens, 100);
    assert_eq!(b.safety_margin_pct, 10);
    assert_eq!(b.image_token_budget, 1000);
    assert_eq!(b.tool_surcharge_tokens, 500);
    assert_eq!(b.web_search_surcharge_tokens, 500);
    assert_eq!(b.code_interpreter_surcharge_tokens, 1000);
}

#[test]
fn quota_defaults_apply() {
    let q = ok(minimal()).quota;
    assert!((q.overshoot_tolerance_factor - 1.10).abs() < 1e-9);
    assert_eq!(q.warning_threshold_pct, 80);
    assert_eq!(q.web_search_max_calls_per_message, 2);
    assert_eq!(q.web_search_daily_quota, 75);
    assert_eq!(q.code_interpreter_max_calls_per_message, 10);
    assert_eq!(q.code_interpreter_daily_quota, 50);
}

#[test]
fn outbox_defaults_apply() {
    let o = ok(minimal()).outbox;
    assert_eq!(o.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(o.cleanup_queue_name, "mini-chat.attachment_cleanup");
    assert_eq!(o.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(o.thread_summary_queue_name, "mini-chat.thread_summary");
    assert_eq!(o.audit_queue_name, "mini-chat.audit");
    assert_eq!(o.num_partitions, 4);
}

#[test]
fn context_and_rag_defaults_apply() {
    let cfg = ok(minimal());
    assert_eq!(cfg.context.recent_messages_limit, 10);
    assert!(!cfg.context.web_search_guard.is_empty());
    assert!(!cfg.context.file_search_guard.is_empty());

    assert_eq!(cfg.rag.max_documents_per_chat, 50);
    assert_eq!(cfg.rag.max_total_upload_mb_per_chat, 100);
    assert_eq!(cfg.rag.uploaded_file_max_size_kb, 25_600);
    assert_eq!(cfg.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(cfg.rag.max_images_per_message, 4);
    assert!(cfg.rag.allow_csv_upload);
    assert_eq!(cfg.rag.max_concurrent_uploads, 10);
}

#[test]
fn thumbnail_and_knowledge_search_defaults_apply() {
    let cfg = ok(minimal());
    assert_eq!(cfg.thumbnail.width, 128);
    assert_eq!(cfg.thumbnail.height, 128);
    assert_eq!(cfg.thumbnail.max_bytes, 131_072);
    assert_eq!(cfg.thumbnail.max_pixels, 100_000_000);
    assert_eq!(cfg.thumbnail.max_decode_bytes, 33_554_432);

    let ks = cfg.knowledge_search;
    assert!(!ks.enabled);
    assert_eq!(ks.max_calls_per_message, 3);
    assert_eq!(ks.top_k, 5);
    assert_eq!(ks.max_chunk_chars, 2000);
    assert!(!ks.guard.is_empty());
}

#[test]
fn worker_defaults_apply() {
    let cfg = ok(minimal());
    assert!(cfg.orphan_watchdog.enabled);
    assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
    assert_eq!(cfg.orphan_watchdog.scan_interval_secs, 60);

    assert!(cfg.upload_reaper.enabled);
    assert_eq!(cfg.upload_reaper.scan_interval_secs, 60);
    assert_eq!(cfg.upload_reaper.stale_after_secs, 300);

    let t = cfg.thread_summary_worker;
    assert!(t.enabled);
    assert_eq!(t.claim_timeout_secs, 300);
    assert_eq!(t.max_attempts, 3);
    assert_eq!(t.compression_threshold_pct, 80);
    assert_eq!(t.message_content_limit, 4000);
    assert_eq!(t.reconcile_interval_secs, 60);
    assert_eq!(t.summary_model_id, "");

    let c = ok(minimal()).cleanup_worker;
    assert_eq!(c.max_attempts, 5);
    assert!(c.enabled);
    assert_eq!(c.poll_interval_secs, 60);
    assert_eq!(c.reconcile_interval_secs, 300);
    assert_eq!(c.stale_in_progress_timeout_secs, 900);
    assert_eq!(c.batch_size, 32);
}

// ── required keys ────────────────────────────────────────────────────────────

#[test]
fn client_credentials_are_required_and_non_empty() {
    assert!(err(json!({})).contains("client_credentials"));
    assert!(err(json!({ "client_credentials": { "client_secret": "s" } })).contains("client_id"));
    assert!(err(json!({ "client_credentials": { "client_id": "c" } })).contains("client_secret"));
    let cases = vec![
        ("client_credentials.client_id", json!(""), "client_id"),
        (
            "client_credentials.client_secret",
            json!(""),
            "client_secret",
        ),
        ("vendor", json!(""), "vendor"),
    ];
    for (path, bad, needle) in cases {
        let message = err(with(path, bad));
        assert!(message.contains(needle), "{path}: {message}");
    }
}

#[test]
fn client_secret_is_redacted_in_debug_output() {
    let cfg = ok(minimal());
    let rendered = format!("{cfg:?}");
    assert!(
        !rendered.contains("s3cret-value"),
        "secret leaked: {rendered}"
    );
    let credentials = format!("{:?}", cfg.client_credentials);
    assert!(!credentials.contains("s3cret-value"));
}

// ── unknown and removed keys (ADR-0010) ──────────────────────────────────────

#[test]
fn unknown_keys_are_rejected_at_the_top_level_and_in_the_strict_sections() {
    let strict = [
        "bogus_key",
        "streaming.bogus_key",
        "estimation_budgets.bogus_key",
        "quota.bogus_key",
        "outbox.bogus_key",
        "context.bogus_key",
        "rag.bogus_key",
        "client_credentials.bogus_key",
        "metrics.bogus_key",
        "thumbnail.bogus_key",
        "knowledge_search.bogus_key",
    ];
    for path in strict {
        let message = err(with(path, json!(1)));
        assert!(message.contains("bogus_key"), "{path}: {message}");
    }
}

#[test]
fn a_misspelled_known_key_fails_startup() {
    let message = err(with("streaming.sse_ping_interval_second", json!(15)));
    assert!(message.contains("sse_ping_interval_second"), "{message}");
}

#[test]
fn unknown_keys_are_rejected_in_provider_entries_and_tenant_overrides() {
    let message = err(with_provider(&json!({ "bogus_key": 1 })));
    assert!(message.contains("bogus_key"), "{message}");

    let overrides =
        json!({ "tenant_overrides": { "t-1": { "host": "h.example.com", "bogus_key": 1 } } });
    let message = err(with_provider(&overrides));
    assert!(message.contains("bogus_key"), "{message}");
}

#[test]
fn unknown_keys_are_ignored_in_the_four_worker_sections() {
    for section in [
        "orphan_watchdog",
        "upload_reaper",
        "thread_summary_worker",
        "cleanup_worker",
    ] {
        let cfg = ok(with(
            &format!("{section}.some_future_key"),
            json!({ "nested": [1, 2, 3] }),
        ));
        assert!(cfg.orphan_watchdog.enabled, "{section}");
    }
}

#[test]
fn removed_keys_fail_startup() {
    let message = err(with("mcp", json!({ "servers": [] })));
    assert!(message.contains("mcp"), "{message}");
    let message = err(with("mcp", json!(true)));
    assert!(message.contains("mcp"), "{message}");

    let message = err(with_provider(
        &json!({ "supports_file_search_filters": true }),
    ));
    assert!(
        message.contains("supports_file_search_filters"),
        "{message}"
    );

    let message = err(with("streaming.web_search_context_size", json!("low")));
    assert!(message.contains("web_search_context_size"), "{message}");
}

// ── ranges ───────────────────────────────────────────────────────────────────

/// `(path, rejected values, accepted values)`.
type RangeCase = (&'static str, Vec<Value>, Vec<Value>);

fn range_table() -> Vec<RangeCase> {
    vec![
        (
            "streaming.sse_ping_interval_seconds",
            vec![json!(4), json!(61)],
            vec![json!(5), json!(60)],
        ),
        (
            "streaming.sse_channel_capacity",
            vec![json!(15), json!(65)],
            vec![json!(16), json!(64)],
        ),
        (
            "quota.overshoot_tolerance_factor",
            vec![json!(0.99), json!(1.51)],
            vec![json!(1.0), json!(1.5)],
        ),
        (
            "quota.warning_threshold_pct",
            vec![json!(0), json!(100)],
            vec![json!(1), json!(99)],
        ),
        (
            "quota.web_search_max_calls_per_message",
            vec![json!(0)],
            vec![json!(1)],
        ),
        (
            "quota.web_search_daily_quota",
            vec![json!(0)],
            vec![json!(1)],
        ),
        (
            "quota.code_interpreter_max_calls_per_message",
            vec![json!(0)],
            vec![json!(1)],
        ),
        (
            "quota.code_interpreter_daily_quota",
            vec![json!(0)],
            vec![json!(1)],
        ),
        (
            "estimation_budgets.minimal_generation_floor",
            vec![json!(0)],
            vec![json!(1), json!(32_768)],
        ),
        (
            "outbox.num_partitions",
            vec![json!(0), json!(3), json!(6), json!(65), json!(128)],
            vec![json!(1), json!(2), json!(4), json!(32), json!(64)],
        ),
        (
            "context.recent_messages_limit",
            vec![json!(101)],
            vec![json!(0), json!(100)],
        ),
        ("rag.max_documents_per_chat", vec![json!(0)], vec![json!(1)]),
        (
            "rag.max_total_upload_mb_per_chat",
            vec![json!(0)],
            vec![json!(1)],
        ),
        (
            "rag.uploaded_file_max_size_kb",
            vec![json!(0)],
            vec![json!(1)],
        ),
        (
            "rag.uploaded_image_max_size_kb",
            vec![json!(0)],
            vec![json!(1)],
        ),
        ("rag.max_images_per_message", vec![json!(0)], vec![json!(1)]),
        (
            "rag.max_concurrent_uploads",
            vec![json!(0), json!(257)],
            vec![json!(1), json!(256)],
        ),
        ("thumbnail.width", vec![json!(0)], vec![json!(1)]),
        ("thumbnail.height", vec![json!(0)], vec![json!(1)]),
        ("thumbnail.max_bytes", vec![json!(0)], vec![json!(1)]),
        ("thumbnail.max_pixels", vec![json!(0)], vec![json!(1)]),
        ("thumbnail.max_decode_bytes", vec![json!(0)], vec![json!(1)]),
        (
            "orphan_watchdog.timeout_secs",
            vec![json!(89), json!(3601)],
            vec![json!(90), json!(3600)],
        ),
        (
            "orphan_watchdog.scan_interval_secs",
            vec![json!(0), json!(3601)],
            vec![json!(1), json!(3600)],
        ),
        (
            "upload_reaper.scan_interval_secs",
            vec![json!(0), json!(3601)],
            vec![json!(1), json!(3600)],
        ),
        (
            "upload_reaper.stale_after_secs",
            vec![json!(59), json!(86_401)],
            vec![json!(60), json!(86_400)],
        ),
        (
            "thread_summary_worker.claim_timeout_secs",
            vec![json!(29), json!(3601)],
            vec![json!(30), json!(3600)],
        ),
        (
            "thread_summary_worker.max_attempts",
            vec![json!(0)],
            vec![json!(1)],
        ),
        (
            "thread_summary_worker.compression_threshold_pct",
            vec![json!(0), json!(100)],
            vec![json!(1), json!(99)],
        ),
        (
            "cleanup_worker.max_attempts",
            vec![json!(0)],
            vec![json!(1)],
        ),
    ]
}

#[test]
fn out_of_range_values_are_rejected_naming_the_key() {
    for (path, rejected, _) in range_table() {
        let leaf = path.rsplit('.').next().unwrap();
        for bad in rejected {
            let message = err(with(path, bad.clone()));
            assert!(message.contains(leaf), "{path}={bad}: {message}");
        }
    }
}

#[test]
fn boundary_values_are_accepted() {
    for (path, _, accepted) in range_table() {
        for good in accepted {
            MiniChatConfig::from_value(with(path, good.clone()))
                .unwrap_or_else(|e| panic!("{path}={good} must be accepted: {e}"));
        }
    }
}

#[test]
fn outbox_queue_names_must_be_non_empty() {
    for key in [
        "queue_name",
        "cleanup_queue_name",
        "chat_cleanup_queue_name",
        "thread_summary_queue_name",
        "audit_queue_name",
    ] {
        let message = err(with(&format!("outbox.{key}"), json!("")));
        assert!(message.contains(key), "{key}: {message}");
    }
}

#[test]
fn minimal_generation_floor_may_not_exceed_max_output_tokens() {
    let mut raw = minimal();
    set(&mut raw, "streaming.max_output_tokens", json!(100));
    set(
        &mut raw,
        "estimation_budgets.minimal_generation_floor",
        json!(101),
    );
    assert!(err(raw).contains("minimal_generation_floor"));

    let mut raw = minimal();
    set(&mut raw, "streaming.max_output_tokens", json!(100));
    set(
        &mut raw,
        "estimation_budgets.minimal_generation_floor",
        json!(100),
    );
    ok(raw);
}

#[test]
fn knowledge_search_requires_store_and_provider_when_enabled() {
    let message = err(with("knowledge_search.enabled", json!(true)));
    assert!(
        message.contains("vector_store_id") || message.contains("provider_id"),
        "{message}"
    );

    let mut raw = minimal();
    set(&mut raw, "knowledge_search.enabled", json!(true));
    set(&mut raw, "knowledge_search.vector_store_id", json!("vs-1"));
    assert!(err(raw).contains("provider_id"));

    let mut raw = minimal();
    set(&mut raw, "knowledge_search.enabled", json!(true));
    set(&mut raw, "knowledge_search.provider_id", json!("main"));
    assert!(err(raw).contains("vector_store_id"));

    let mut raw = minimal();
    set(&mut raw, "knowledge_search.enabled", json!(true));
    set(&mut raw, "knowledge_search.vector_store_id", json!("vs-1"));
    set(&mut raw, "knowledge_search.provider_id", json!("main"));
    let ks = ok(raw).knowledge_search;
    assert_eq!(ks.vector_store_id.as_deref(), Some("vs-1"));
    assert_eq!(ks.provider_id.as_deref(), Some("main"));
}

#[test]
fn knowledge_search_numeric_knobs_must_be_positive() {
    for key in ["max_calls_per_message", "top_k", "max_chunk_chars"] {
        let message = err(with(&format!("knowledge_search.{key}"), json!(0)));
        assert!(message.contains(key), "{key}: {message}");
    }
}

// ── deprecated fields ────────────────────────────────────────────────────────

#[test]
fn deprecated_fields_at_their_defaults_log_no_warning() {
    assert!(ok(minimal()).deprecated_field_warnings().is_empty());

    let mut raw = minimal();
    set(&mut raw, "estimation_budgets.safety_margin_pct", json!(10));
    set(&mut raw, "cleanup_worker.batch_size", json!(32));
    assert!(ok(raw).deprecated_field_warnings().is_empty());
}

#[test]
fn each_deprecated_field_set_to_a_non_default_value_logs_exactly_one_warning() {
    let cases = vec![
        ("estimation_budgets.bytes_per_token_conservative", json!(3)),
        ("estimation_budgets.fixed_overhead_tokens", json!(1)),
        ("estimation_budgets.safety_margin_pct", json!(25)),
        ("estimation_budgets.image_token_budget", json!(7)),
        ("estimation_budgets.tool_surcharge_tokens", json!(7)),
        ("estimation_budgets.web_search_surcharge_tokens", json!(7)),
        (
            "estimation_budgets.code_interpreter_surcharge_tokens",
            json!(7),
        ),
        ("cleanup_worker.enabled", json!(false)),
        ("cleanup_worker.poll_interval_secs", json!(5)),
        ("cleanup_worker.reconcile_interval_secs", json!(5)),
        ("cleanup_worker.stale_in_progress_timeout_secs", json!(5)),
        ("cleanup_worker.batch_size", json!(8)),
        ("thread_summary_worker.reconcile_interval_secs", json!(5)),
    ];
    for (path, value) in cases {
        let warnings = ok(with(path, value)).deprecated_field_warnings();
        assert_eq!(warnings.len(), 1, "{path}: {warnings:?}");
        assert!(warnings[0].contains(path), "{path}: {warnings:?}");
    }
}

#[test]
fn deprecated_fields_are_parsed_but_not_validated() {
    let mut raw = minimal();
    set(
        &mut raw,
        "estimation_budgets.bytes_per_token_conservative",
        json!(0),
    );
    set(&mut raw, "cleanup_worker.batch_size", json!(0));
    let cfg = ok(raw);
    assert_eq!(cfg.estimation_budgets.bytes_per_token_conservative, 0);
    assert_eq!(cfg.deprecated_field_warnings().len(), 2);
}

// ── providers ────────────────────────────────────────────────────────────────

#[test]
fn a_minimal_provider_entry_is_accepted_with_its_defaults() {
    let cfg = ok(with_provider(&json!({})));
    let entry = &cfg.providers["main"];
    assert_eq!(entry.kind, ProviderKind::OpenaiResponses);
    assert_eq!(entry.storage_kind, StorageKind::Openai);
    assert_eq!(entry.host, "api.openai.com");
    assert!(!entry.use_http);
    assert_eq!(entry.api_path, "/v1/responses");
    assert_eq!(entry.effective_port(), 443);
}

#[test]
fn provider_kind_and_storage_kind_enumerations_are_enforced() {
    let kinds = vec![
        ("openai_responses", ProviderKind::OpenaiResponses),
        (
            "openai_chat_completions",
            ProviderKind::OpenaiChatCompletions,
        ),
        ("vllm_responses", ProviderKind::VllmResponses),
        ("anthropic_messages", ProviderKind::AnthropicMessages),
    ];
    for (raw, expected) in kinds {
        let cfg = ok(with_provider(&json!({ "kind": raw })));
        assert_eq!(cfg.providers["main"].kind, expected, "{raw}");
    }
    // the message names the rejected value or the key
    let message = err(with_provider(&json!({ "kind": "gemini" })));
    assert!(
        message.contains("gemini") || message.contains("kind"),
        "{message}"
    );
    let message = err(with_provider(&json!({ "storage_kind": "s3" })));
    assert!(
        message.contains("s3") || message.contains("storage_kind"),
        "{message}"
    );
}

#[test]
fn provider_required_keys_are_enforced() {
    for key in ["kind", "host", "storage_kind"] {
        let mut entry = provider(&json!({}));
        entry.as_object_mut().unwrap().remove(key);
        let message = err(with("providers.main", entry));
        assert!(message.contains(key), "{key}: {message}");
    }
    assert!(err(with_provider(&json!({ "host": "" }))).contains("host"));
}

#[test]
fn provider_host_allows_only_alias_safe_characters() {
    let good = [
        "api.openai.com",
        "my-host_1.example.com:8443",
        "[::1]",
        "localhost",
    ];
    for host in good {
        ok(with_provider(&json!({ "host": host })));
    }
    let bad = [
        "a/b",
        "a?b",
        "a#b",
        "user@host",
        "a b",
        "host\n",
        "a\\b",
        "a%2fb",
    ];
    for host in bad {
        let message = err(with_provider(&json!({ "host": host })));
        assert!(message.contains("host"), "{host:?}: {message}");
    }
}

#[test]
fn provider_port_rules() {
    assert!(err(with_provider(&json!({ "port": 0 }))).contains("port"));
    assert_eq!(
        ok(with_provider(&json!({ "port": 8443 }))).providers["main"].effective_port(),
        8443
    );
    assert_eq!(
        ok(with_provider(&json!({ "use_http": true }))).providers["main"].effective_port(),
        80
    );
    assert_eq!(
        ok(with_provider(&json!({ "use_http": true, "port": 8080 }))).providers["main"]
            .effective_port(),
        8080
    );
}

#[test]
fn azure_storage_requires_a_clean_api_version() {
    let azure = |version: Value| {
        let mut extra = json!({ "storage_kind": "azure" });
        if !version.is_null() {
            extra["api_version"] = version;
        }
        with_provider(&extra)
    };
    assert!(err(azure(Value::Null)).contains("api_version"));
    assert!(err(azure(json!(""))).contains("api_version"));
    assert!(err(azure(json!("   "))).contains("api_version"));
    for bad in ["2025/03/01", "2025-03-01&x=1", "2025 03", "v1?x"] {
        assert!(err(azure(json!(bad))).contains("api_version"), "{bad}");
    }
    for good in ["2025-03-01-preview", "2024.10.21", "v1"] {
        ok(azure(json!(good)));
    }
    // the openai storage kind does not need one
    ok(with_provider(&json!({ "storage_kind": "openai" })));
}

#[test]
fn a_dangling_rag_provider_reference_fails_startup_naming_it() {
    let message = err(with_provider(&json!({ "rag_provider": "ghost" })));
    assert!(message.contains("ghost"), "{message}");

    let mut raw = minimal();
    set(
        &mut raw,
        "providers.main",
        provider(&json!({ "rag_provider": "files" })),
    );
    set(&mut raw, "providers.files", provider(&json!({})));
    ok(raw);
}

#[test]
fn tenant_overrides_need_a_host_or_an_alias_and_a_clean_host() {
    let with_override =
        |entry: Value| with_provider(&json!({ "tenant_overrides": { "tenant-a": entry } }));

    assert!(err(with_override(json!({}))).contains("tenant-a"));
    assert!(err(with_override(json!({ "auth_plugin_type": "x" }))).contains("tenant-a"));
    ok(with_override(json!({ "host": "tenant-a.example.com" })));
    // an IP host accepts any explicit alias
    ok(with_override(
        json!({ "host": "10.0.0.7", "upstream_alias": "tenant-a-alias" }),
    ));
    let message = err(with_override(json!({ "host": "a/b" })));
    assert!(message.contains("host"), "{message}");
}

#[test]
fn an_explicit_alias_must_equal_the_oagw_derived_alias_for_a_hostname_endpoint() {
    // hostname + standard port: the derived alias is the host
    ok(with_provider(
        &json!({ "upstream_alias": "api.openai.com" }),
    ));
    ok(with_provider(
        &json!({ "upstream_alias": "API.OpenAI.com." }),
    ));
    let message = err(with_provider(&json!({ "upstream_alias": "openai" })));
    assert!(
        message.contains("providers.main.upstream_alias") && message.contains("api.openai.com"),
        "{message}"
    );
    // hostname + non-standard port: the derived alias carries the port
    let local = |alias: &str| {
        with_provider(&json!({
            "host": "localhost", "port": 9999, "use_http": true, "upstream_alias": alias
        }))
    };
    ok(local("localhost:9999"));
    assert!(err(local("localhost")).contains("localhost:9999"));
    // `use_http` makes 80 the standard port, 443 a non-standard one
    ok(with_provider(
        &json!({ "host": "llm.internal", "use_http": true, "upstream_alias": "llm.internal" }),
    ));
    ok(with_provider(&json!({
        "host": "llm.internal", "port": 443, "use_http": true, "upstream_alias": "llm.internal:443"
    })));
    // an IP host accepts any explicit alias
    ok(with_provider(
        &json!({ "host": "127.0.0.1", "port": 9999, "use_http": true, "upstream_alias": "mock" }),
    ));
}

#[test]
fn an_override_alias_is_checked_against_the_alias_derived_for_its_host() {
    let with_override =
        |entry: Value| with_provider(&json!({ "tenant_overrides": { "tenant-a": entry } }));
    ok(with_override(
        json!({ "host": "tenant-a.example.com", "upstream_alias": "tenant-a.example.com" }),
    ));
    let message = err(with_override(
        json!({ "host": "tenant-a.example.com", "upstream_alias": "tenant-a" }),
    ));
    assert!(
        message.contains("tenant_overrides.tenant-a.upstream_alias")
            && message.contains("tenant-a.example.com"),
        "{message}"
    );
    // without its own host the override is checked against the entry host
    assert!(
        err(with_override(json!({ "upstream_alias": "tenant-a-alias" })))
            .contains("tenant_overrides.tenant-a.upstream_alias")
    );
    ok(with_override(json!({ "upstream_alias": "api.openai.com" })));
}

// ── ${VAR} expansion ─────────────────────────────────────────────────────────

fn expanded_fields_config() -> Value {
    let mut raw = json!({
        "vendor": "${MC_CFG_VENDOR:-fallback-vendor}",
        "url_prefix": "${MC_CFG_PREFIX:-/mini-chat}",
        "client_credentials": { "client_id": "${MC_CFG_ID}", "client_secret": "${MC_CFG_SECRET}" }
    });
    set(
        &mut raw,
        "providers.main",
        provider(&json!({
            "host": "${MC_CFG_HOST}",
            "auth_config": { "header": "api-key", "secret_ref": "${MC_CFG_REF}" },
            "tenant_overrides": {
                "tenant-a": { "host": "${MC_CFG_HOST}", "auth_config": { "secret_ref": "${MC_CFG_REF}" } }
            }
        })),
    );
    raw
}

#[test]
fn expansion_applies_to_provider_host_auth_config_overrides_and_credentials() {
    temp_env::with_vars(
        [
            ("MC_CFG_HOST", Some("api.example.com")),
            ("MC_CFG_REF", Some("secret-key")),
            ("MC_CFG_ID", Some("client-1")),
            ("MC_CFG_SECRET", Some("shh")),
        ],
        || {
            let mut cfg = MiniChatConfig::parse(expanded_fields_config()).expect("parse");
            cfg.expand_vars().expect("expand");
            cfg.validate().expect("valid after expansion");
            let entry = &cfg.providers["main"];
            assert_eq!(entry.host, "api.example.com");
            assert_eq!(entry.auth_config["secret_ref"], "secret-key");
            let tenant = &entry.tenant_overrides["tenant-a"];
            assert_eq!(tenant.host.as_deref(), Some("api.example.com"));
            assert_eq!(
                tenant.auth_config.as_ref().unwrap()["secret_ref"],
                "secret-key"
            );
            assert_eq!(cfg.client_credentials.client_id, "client-1");
            assert_eq!(cfg.client_credentials.client_secret.expose_secret(), "shh");
        },
    );
}

#[test]
fn expansion_does_not_touch_other_fields() {
    temp_env::with_vars(
        [
            ("MC_CFG_HOST", Some("api.example.com")),
            ("MC_CFG_REF", Some("k")),
            ("MC_CFG_ID", Some("c")),
            ("MC_CFG_SECRET", Some("s")),
            ("MC_CFG_VENDOR", Some("from-env")),
            ("MC_CFG_PREFIX", Some("/from-env")),
        ],
        || {
            let mut cfg = MiniChatConfig::parse(expanded_fields_config()).expect("parse");
            cfg.expand_vars().expect("expand");
            assert_eq!(cfg.vendor, "${MC_CFG_VENDOR:-fallback-vendor}");
            assert_eq!(cfg.url_prefix, "${MC_CFG_PREFIX:-/mini-chat}");
        },
    );
}

#[test]
fn an_unset_variable_without_a_default_is_a_hard_error() {
    temp_env::with_vars(
        [
            ("MC_CFG_HOST", None::<&str>),
            ("MC_CFG_REF", Some("k")),
            ("MC_CFG_ID", Some("c")),
            ("MC_CFG_SECRET", Some("s")),
        ],
        || {
            let mut cfg = MiniChatConfig::parse(expanded_fields_config()).expect("parse");
            assert!(cfg.expand_vars().is_err());
        },
    );
}

#[test]
fn a_default_value_is_used_for_an_unset_variable() {
    temp_env::with_vars([("MC_CFG_HOST", None::<&str>)], || {
        let raw = with_provider(&json!({ "host": "${MC_CFG_HOST:-fallback.example.com}" }));
        let mut cfg = MiniChatConfig::parse(raw).expect("parse");
        cfg.expand_vars().expect("expand");
        assert_eq!(cfg.providers["main"].host, "fallback.example.com");
    });
}

#[test]
fn the_host_character_check_runs_after_expansion() {
    temp_env::with_vars([("MC_CFG_HOST", Some("evil.example.com/path?x=1"))], || {
        let raw = with_provider(&json!({ "host": "${MC_CFG_HOST}" }));
        let mut cfg = MiniChatConfig::parse(raw).expect("parse accepts the unexpanded placeholder");
        cfg.expand_vars().expect("expand");
        let message = cfg
            .validate()
            .expect_err("expanded host must fail the character check");
        assert!(message.to_string().contains("host"));
    });
}
