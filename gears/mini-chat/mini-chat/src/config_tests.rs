#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreadable_literal,
    clippy::cognitive_complexity
)]

use serde_json::json;

use super::*;

fn base() -> serde_json::Value {
    json!({ "client_credentials": { "client_id": "a", "client_secret": "b" } })
}

fn parse(v: serde_json::Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(v)
}

fn approx(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn defaults_match_design() {
    let c = parse(base()).unwrap();
    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.vendor, "constructorfabric");
    assert_eq!(c.client_credentials.client_id, "a");
    assert_eq!(c.metrics.prefix, "");
    assert_eq!(c.metrics.effective_prefix(), "mini_chat");

    assert_eq!(c.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(c.streaming.sse_channel_capacity, 32);
    assert_eq!(c.streaming.max_output_tokens, 32768);

    assert_eq!(c.estimation_budgets.minimal_generation_floor, 50);

    assert!(approx(c.quota.overshoot_tolerance_factor, 1.10));
    assert_eq!(c.quota.warning_threshold_pct, 80);
    assert_eq!(c.quota.web_search_max_calls_per_message, 2);
    assert_eq!(c.quota.web_search_daily_quota, 75);
    assert_eq!(c.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(c.quota.code_interpreter_daily_quota, 50);

    assert_eq!(c.outbox.num_partitions, 4);
    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(c.outbox.cleanup_queue_name, "mini-chat.attachment_cleanup");
    assert_eq!(c.outbox.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(
        c.outbox.thread_summary_queue_name,
        "mini-chat.thread_summary"
    );
    assert_eq!(c.outbox.audit_queue_name, "mini-chat.audit");

    assert_eq!(c.context.recent_messages_limit, 10);
    assert!(
        c.context
            .web_search_guard
            .starts_with("Use web_search only if")
    );
    assert!(!c.context.file_search_guard.is_empty());

    assert_eq!(c.rag.uploaded_file_max_size_kb, 25600);
    assert_eq!(c.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert_eq!(c.rag.max_documents_per_chat, 50);
    assert_eq!(c.rag.max_total_upload_mb_per_chat, 100);
    assert!(c.rag.allow_csv_upload);
    assert_eq!(c.rag.max_concurrent_uploads, 10);

    assert_eq!(c.thumbnail.width, 128);
    assert_eq!(c.thumbnail.height, 128);
    assert_eq!(c.thumbnail.max_bytes, 131_072);
    assert_eq!(c.thumbnail.max_pixels, 100_000_000);
    assert_eq!(c.thumbnail.max_decode_bytes, 33_554_432);

    assert!(c.orphan_watchdog.enabled);
    assert_eq!(c.orphan_watchdog.timeout_secs, 300);
    assert_eq!(c.orphan_watchdog.scan_interval_secs, 60);

    assert!(c.upload_reaper.enabled);
    assert_eq!(c.upload_reaper.scan_interval_secs, 60);
    assert_eq!(c.upload_reaper.stale_after_secs, 300);

    assert!(c.thread_summary_worker.enabled);
    assert_eq!(c.thread_summary_worker.claim_timeout_secs, 300);
    assert_eq!(c.thread_summary_worker.max_attempts, 3);
    assert_eq!(c.thread_summary_worker.compression_threshold_pct, 80);
    assert_eq!(c.thread_summary_worker.summary_model_id, "");
    assert_eq!(
        c.thread_summary_worker.effective_summary_model_id(),
        "gpt-4.1-mini"
    );
    assert_eq!(c.thread_summary_worker.message_content_limit, 4000);
    assert!(
        c.thread_summary_worker
            .summary_system_prompt
            .starts_with("You are a conversation summarizer.")
    );

    assert_eq!(c.cleanup_worker.max_attempts, 5);

    assert!(!c.knowledge_search.enabled);
    assert_eq!(c.knowledge_search.max_calls_per_message, 3);
    assert_eq!(c.knowledge_search.top_k, 5);
    assert_eq!(c.knowledge_search.max_chunk_chars, 2000);

    let openai = c.providers.get("openai").expect("default openai provider");
    assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
    assert_eq!(openai.host, "api.openai.com");
    assert_eq!(openai.storage_kind, StorageKind::Openai);
    assert_eq!(openai.api_path, "/v1/responses");
    assert_eq!(openai.effective_port(), 443);
    assert_eq!(c.providers.len(), 1);
}

#[test]
fn defaults_validate_and_fill_alias() {
    let mut c = parse(base()).unwrap();
    c.validate().unwrap();
    assert_eq!(
        c.providers["openai"].upstream_alias.as_deref(),
        Some("api.openai.com")
    );
    assert_eq!(
        c.providers["openai"].effective_storage_backend("openai"),
        "openai"
    );
}

fn dev_config_block() -> serde_json::Value {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../config/mini-chat.yaml"
    );
    let text = std::fs::read_to_string(path).expect("read config/mini-chat.yaml");
    let root: serde_json::Value = serde_saphyr::from_str(&text).expect("parse yaml");
    root["gears"]["mini-chat"]["config"].clone()
}

#[test]
fn parses_repo_dev_config() {
    temp_env::with_var(
        "AZURE_OPENAI_API_HOST",
        Some("example.openai.azure.com"),
        || {
            let mut c = parse(dev_config_block()).unwrap();
            c.validate().unwrap();
            let p = &c.providers["azure_openai"];
            assert_eq!(
                p.upstream_alias.as_deref(),
                Some("example.openai.azure.com")
            );
            assert_eq!(p.host, "example.openai.azure.com");
            assert_eq!(p.api_version.as_deref(), Some("2025-03-01-preview"));
            assert_eq!(p.storage_kind, StorageKind::Azure);
            assert_eq!(c.orphan_watchdog.timeout_secs, 90);
            assert_eq!(c.orphan_watchdog.scan_interval_secs, 10);
            assert_eq!(c.thread_summary_worker.summary_model_id, "gpt-4.1-mini");
            assert!(!c.providers.contains_key("openai"));
        },
    );
}

#[test]
fn expands_client_credentials_and_auth_config() {
    temp_env::with_vars(
        [
            ("MC_TEST_SECRET", Some("s3cret")),
            ("MC_TEST_KEY_REF", Some("my-key")),
        ],
        || {
            let mut v = base();
            v["client_credentials"]["client_secret"] = json!("${MC_TEST_SECRET}");
            v["providers"] = json!({
                "p": {
                    "kind": "openai_responses",
                    "storage_kind": "openai",
                    "host": "api.example.com",
                    "auth_config": { "secret_ref": "${MC_TEST_KEY_REF}" },
                    "tenant_overrides": {
                        "11111111-1111-1111-1111-111111111111": { "host": "t.example.com" }
                    }
                }
            });
            let mut c = parse(v).unwrap();
            c.validate().unwrap();
            assert_eq!(c.client_credentials.secret(), "s3cret");
            let dbg = format!("{:?}", c.client_credentials);
            assert!(!dbg.contains("s3cret"), "secret must be redacted: {dbg}");
            let p = &c.providers["p"];
            assert_eq!(
                p.auth_config
                    .as_ref()
                    .unwrap()
                    .get("secret_ref")
                    .map(String::as_str),
                Some("my-key")
            );
            let o = &p.tenant_overrides["11111111-1111-1111-1111-111111111111"];
            assert_eq!(o.upstream_alias.as_deref(), Some("t.example.com"));
        },
    );
}

#[test]
fn rejects_out_of_range() {
    let azure_no_version = json!({
        "kind": "openai_responses", "storage_kind": "azure", "host": "x.openai.azure.com"
    });
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "ping=4",
            json!({ "streaming": { "sse_ping_interval_seconds": 4 } }),
        ),
        (
            "capacity=65",
            json!({ "streaming": { "sse_channel_capacity": 65 } }),
        ),
        (
            "orphan timeout=89",
            json!({ "orphan_watchdog": { "timeout_secs": 89 } }),
        ),
        (
            "reaper stale=59",
            json!({ "upload_reaper": { "stale_after_secs": 59 } }),
        ),
        ("partitions=3", json!({ "outbox": { "num_partitions": 3 } })),
        (
            "overshoot=1.6",
            json!({ "quota": { "overshoot_tolerance_factor": 1.6 } }),
        ),
        (
            "warning=0",
            json!({ "quota": { "warning_threshold_pct": 0 } }),
        ),
        (
            "floor=0",
            json!({ "estimation_budgets": { "minimal_generation_floor": 0 } }),
        ),
        (
            "floor>max_output",
            json!({
                "streaming": { "max_output_tokens": 100 },
                "estimation_budgets": { "minimal_generation_floor": 101 }
            }),
        ),
        (
            "azure without api_version",
            json!({ "providers": { "az": azure_no_version } }),
        ),
        (
            "rag_provider nope",
            json!({ "providers": { "openai": {
            "kind": "anthropic_messages", "storage_kind": "openai",
            "host": "api.anthropic.com", "rag_provider": "nope"
        } } }),
        ),
        (
            "host a/b",
            json!({ "providers": { "openai": {
            "kind": "openai_responses", "storage_kind": "openai", "host": "a/b"
        } } }),
        ),
        ("empty vendor", json!({ "vendor": "" })),
        (
            "empty client_id",
            json!({ "client_credentials": { "client_id": "", "client_secret": "b" } }),
        ),
        (
            "empty queue name",
            json!({ "outbox": { "audit_queue_name": "" } }),
        ),
        (
            "knowledge enabled without ids",
            json!({ "knowledge_search": { "enabled": true } }),
        ),
        (
            "override without host/alias",
            json!({ "providers": { "openai": {
            "kind": "openai_responses", "storage_kind": "openai", "host": "api.openai.com",
            "tenant_overrides": { "11111111-1111-1111-1111-111111111111": { "auth_plugin_type": "x" } }
        } } }),
        ),
        (
            "azure api_version charset",
            json!({ "providers": { "az": {
            "kind": "openai_responses", "storage_kind": "azure", "host": "x.openai.azure.com",
            "api_version": "2025?x=1"
        } } }),
        ),
        (
            "recent_messages_limit=101",
            json!({ "context": { "recent_messages_limit": 101 } }),
        ),
        (
            "max_concurrent_uploads=0",
            json!({ "rag": { "max_concurrent_uploads": 0 } }),
        ),
        (
            "claim_timeout=29",
            json!({ "thread_summary_worker": { "claim_timeout_secs": 29 } }),
        ),
        (
            "cleanup max_attempts=0",
            json!({ "cleanup_worker": { "max_attempts": 0 } }),
        ),
    ];
    for (name, patch) in cases {
        let mut v = base();
        for (k, val) in patch.as_object().unwrap() {
            v[k] = val.clone();
        }
        let mut c = parse(v).unwrap_or_else(|e| panic!("{name}: must deserialize: {e}"));
        assert!(c.validate().is_err(), "{name}: validate() must fail");
    }
}

#[test]
fn rejects_unknown_keys() {
    let mut v = base();
    v["mcp"] = json!({});
    assert!(parse(v).is_err(), "top-level mcp must be rejected");

    let mut v = base();
    v["streaming"] = json!({ "web_search_context_size": "low" });
    assert!(
        parse(v).is_err(),
        "streaming.web_search_context_size must be rejected"
    );

    let mut v = base();
    v["providers"] = json!({ "x": {
        "kind": "openai_responses", "storage_kind": "openai", "host": "h",
        "supports_file_search_filters": true
    } });
    assert!(
        parse(v).is_err(),
        "providers.x.supports_file_search_filters must be rejected"
    );

    let mut v = base();
    v["orphan_watchdog"] = json!({ "foo": 1 });
    let mut c = parse(v).expect("worker sections accept unknown keys");
    c.validate().unwrap();
}

// ---------------------------------------------------------------------------
// OAGW upstream aliases (review fix round 1)
// ---------------------------------------------------------------------------

fn validated_providers(providers: serde_json::Value) -> anyhow::Result<MiniChatConfig> {
    let mut v = base();
    v["providers"] = providers;
    let mut c = parse(v)?;
    c.validate()?;
    Ok(c)
}

#[test]
fn derived_alias_mirrors_oagw() {
    // Normalized host; `:port` unless it is the scheme's standard port.
    assert_eq!(
        oagw_derived_alias("api.openai.com", 443, false),
        "api.openai.com"
    );
    assert_eq!(
        oagw_derived_alias("LocalHost.", 8080, true),
        "localhost:8080"
    );
    assert_eq!(oagw_derived_alias("svc", 80, true), "svc");
    assert_eq!(oagw_derived_alias("svc", 443, true), "svc:443");
    assert_eq!(oagw_derived_alias("svc", 80, false), "svc:80");
    assert_eq!(
        oagw_derived_alias("127.0.0.1", 8443, true),
        "127.0.0.1:8443"
    );
    assert_eq!(oagw_derived_alias("127.0.0.1", 443, false), "127.0.0.1");
}

#[test]
fn default_alias_is_oagw_derivation() {
    let c = validated_providers(json!({
        "mock": {
            "kind": "openai_responses", "storage_kind": "openai",
            "host": "localhost", "port": 8080, "use_http": true,
            "tenant_overrides": {
                "11111111-1111-1111-1111-111111111111": { "host": "t.local" }
            }
        }
    }))
    .unwrap();
    let p = &c.providers["mock"];
    assert_eq!(p.upstream_alias.as_deref(), Some("localhost:8080"));
    let o = &p.tenant_overrides["11111111-1111-1111-1111-111111111111"];
    assert_eq!(o.upstream_alias.as_deref(), Some("t.local:8080"));
}

#[test]
fn explicit_alias_on_hostname_is_replaced_by_derived() {
    let c = validated_providers(json!({
        "custom": {
            "kind": "openai_responses", "storage_kind": "openai",
            "host": "api.example.com", "upstream_alias": "my-alias",
            "tenant_overrides": {
                "11111111-1111-1111-1111-111111111111": { "upstream_alias": "tenant-x" }
            }
        },
        "same": {
            "kind": "openai_responses", "storage_kind": "openai",
            "host": "other.example.com", "upstream_alias": "OTHER.example.com."
        }
    }))
    .unwrap();
    let p = &c.providers["custom"];
    assert_eq!(p.upstream_alias.as_deref(), Some("api.example.com"));
    // Override without host: derived from the entry host.
    let o = &p.tenant_overrides["11111111-1111-1111-1111-111111111111"];
    assert_eq!(o.upstream_alias.as_deref(), Some("api.example.com"));
    // An explicit alias equal to the derived one (after normalization) is kept.
    assert_eq!(
        c.providers["same"].upstream_alias.as_deref(),
        Some("other.example.com")
    );
}

#[test]
fn explicit_alias_on_ip_host_is_kept() {
    let c = validated_providers(json!({
        "mock": {
            "kind": "openai_responses", "storage_kind": "openai",
            "host": "127.0.0.1", "port": 9000, "use_http": true,
            "upstream_alias": "Mock-Provider"
        },
        "plain": {
            "kind": "openai_responses", "storage_kind": "openai", "host": "10.0.0.5"
        }
    }))
    .unwrap();
    assert_eq!(
        c.providers["mock"].upstream_alias.as_deref(),
        Some("mock-provider")
    );
    assert_eq!(
        c.providers["plain"].upstream_alias.as_deref(),
        Some("10.0.0.5")
    );
}

#[test]
fn shared_alias_with_different_upstream_is_accepted_and_reported() {
    let entry = |secret: &str| {
        json!({
            "kind": "openai_responses", "storage_kind": "openai", "host": "api.example.com",
            "auth_plugin_type": "plugin", "auth_config": { "secret_ref": secret }
        })
    };
    // Same alias, same endpoint and credentials: one upstream, nothing to report.
    let c = validated_providers(json!({"a": entry("cred://k"), "b": entry("cred://k")})).unwrap();
    assert!(shared_alias_conflicts(&c.providers).is_empty());

    // Two entries, different credentials: accepted (OAGW reuses the
    // upstream); the first entry in id order owns it.
    let c =
        validated_providers(json!({"b": entry("cred://k2"), "a": entry("cred://k1")})).unwrap();
    assert_eq!(
        shared_alias_conflicts(&c.providers),
        vec![AliasConflict {
            alias: "api.example.com".into(),
            owner: "a".into(),
            other: "b".into(),
        }]
    );

    // Override on the same host with other credentials.
    let mut e = entry("cred://k1");
    e["tenant_overrides"] = json!({
        "11111111-1111-1111-1111-111111111111": {
            "host": "API.example.com", "auth_config": { "secret_ref": "cred://tenant" }
        }
    });
    let c = validated_providers(json!({"a": e})).unwrap();
    assert_eq!(
        shared_alias_conflicts(&c.providers),
        vec![AliasConflict {
            alias: "api.example.com".into(),
            owner: "a".into(),
            other: "a.tenant_overrides.11111111-1111-1111-1111-111111111111".into(),
        }]
    );

    // Two entries on the same mock listener (IP host, same alias), one with
    // credentials and one without.
    let ip = |port: u16, auth: bool| {
        let mut v = json!({
            "kind": "openai_responses", "storage_kind": "openai", "host": "127.0.0.1",
            "port": port, "use_http": true, "upstream_alias": "mock"
        });
        if auth {
            v["auth_plugin_type"] = json!("plugin");
            v["auth_config"] = json!({ "secret_ref": "cred://k" });
        }
        v
    };
    let c = validated_providers(json!({"openai": ip(9000, true), "azure_openai": ip(9000, false)}))
        .unwrap();
    assert_eq!(
        shared_alias_conflicts(&c.providers),
        vec![AliasConflict {
            alias: "mock".into(),
            owner: "azure_openai".into(),
            other: "openai".into(),
        }]
    );
    // Different ports under one explicit alias.
    let c = validated_providers(json!({"a": ip(9000, false), "b": ip(9001, false)})).unwrap();
    assert_eq!(shared_alias_conflicts(&c.providers).len(), 1);
}
