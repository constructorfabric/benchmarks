use serde_json::json;

use super::*;

fn parse(v: serde_json::Value) -> Result<MiniChatConfig, serde_json::Error> {
    serde_json::from_value(v)
}

fn valid() -> serde_json::Value {
    json!({"client_credentials": {"client_id": "mini-chat", "client_secret": "s3cret"}})
}

fn with(mut base: serde_json::Value, path: &[&str], value: serde_json::Value) -> serde_json::Value {
    let Some((last, parents)) = path.split_last() else {
        return base;
    };
    let mut cur = &mut base;
    for k in parents {
        if cur.get(*k).is_none() {
            cur[*k] = json!({});
        }
        cur = cur.get_mut(*k).unwrap();
    }
    cur[*last] = value;
    base
}

#[test]
// One flat assertion per documented default; splitting would only scatter the list.
#[allow(clippy::cognitive_complexity)]
fn defaults_match_design() {
    let c = parse(valid()).unwrap();
    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.vendor, "constructorfabric");
    assert_eq!(c.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(c.streaming.sse_channel_capacity, 32);
    assert_eq!(c.streaming.max_output_tokens, 32_768);
    assert_eq!(c.estimation_budgets.minimal_generation_floor, 50);
    assert!((c.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(c.quota.warning_threshold_pct, 80);
    assert_eq!(c.quota.web_search_max_calls_per_message, 2);
    assert_eq!(c.quota.web_search_daily_quota, 75);
    assert_eq!(c.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(c.quota.code_interpreter_daily_quota, 50);
    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(c.outbox.cleanup_queue_name, "mini-chat.attachment_cleanup");
    assert_eq!(c.outbox.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(
        c.outbox.thread_summary_queue_name,
        "mini-chat.thread_summary"
    );
    assert_eq!(c.outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(c.outbox.num_partitions, 4);
    assert_eq!(c.context.recent_messages_limit, 10);
    assert_eq!(c.rag.max_documents_per_chat, 50);
    assert_eq!(c.rag.max_total_upload_mb_per_chat, 100);
    assert!(c.rag.allow_csv_upload);
    assert_eq!(c.rag.max_concurrent_uploads, 10);
    assert_eq!(c.rag.uploaded_file_max_size_kb, 25_600);
    assert_eq!(c.rag.uploaded_image_max_size_kb, 5_120);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert_eq!((c.thumbnail.width, c.thumbnail.height), (128, 128));
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
    assert_eq!(
        c.thread_summary_worker.effective_summary_model_id(),
        DEFAULT_SUMMARY_MODEL_ID
    );
    assert_eq!(c.thread_summary_worker.message_content_limit, 4_000);
    assert_eq!(c.cleanup_worker.max_attempts, 5);
    assert!(!c.knowledge_search.enabled);
    let p = c.providers.get("openai").expect("default provider");
    assert_eq!(p.host, "api.openai.com");
    assert_eq!(p.kind, ProviderKind::OpenaiResponses);
    assert_eq!(p.storage_kind, StorageKind::Openai);
    assert_eq!(p.api_path, "/v1/responses");
    assert_eq!(p.effective_port(), 443);
    c.validate().unwrap();
}

#[test]
fn unknown_root_key_is_rejected() {
    assert!(parse(with(valid(), &["nope"], json!(1))).is_err());
    assert!(parse(with(valid(), &["streaming", "nope"], json!(1))).is_err());
    assert!(parse(with(valid(), &["rag", "nope"], json!(1))).is_err());
}

#[test]
fn worker_sections_accept_unknown_keys() {
    for section in [
        "orphan_watchdog",
        "upload_reaper",
        "thread_summary_worker",
        "cleanup_worker",
    ] {
        let c = parse(with(valid(), &[section, "legacy_key"], json!(true)));
        assert!(c.is_ok(), "{section}: {c:?}");
    }
}

#[test]
fn client_credentials_are_required() {
    let c = parse(json!({})).unwrap();
    assert!(c.validate().is_err());
}

#[test]
fn ranges_are_validated() {
    let cases: Vec<(&[&str], serde_json::Value)> = vec![
        (&["streaming", "sse_ping_interval_seconds"], json!(4)),
        (&["streaming", "sse_ping_interval_seconds"], json!(61)),
        (&["streaming", "sse_channel_capacity"], json!(15)),
        (&["streaming", "sse_channel_capacity"], json!(65)),
        (
            &["estimation_budgets", "minimal_generation_floor"],
            json!(0),
        ),
        (&["quota", "overshoot_tolerance_factor"], json!(1.6)),
        (&["quota", "overshoot_tolerance_factor"], json!(0.9)),
        (&["quota", "warning_threshold_pct"], json!(0)),
        (&["quota", "warning_threshold_pct"], json!(100)),
        (&["quota", "web_search_daily_quota"], json!(0)),
        (&["outbox", "num_partitions"], json!(3)),
        (&["outbox", "num_partitions"], json!(128)),
        (&["context", "recent_messages_limit"], json!(101)),
        (&["rag", "max_concurrent_uploads"], json!(0)),
        (&["rag", "max_concurrent_uploads"], json!(257)),
        (&["orphan_watchdog", "timeout_secs"], json!(89)),
        (&["orphan_watchdog", "timeout_secs"], json!(3601)),
        (&["orphan_watchdog", "scan_interval_secs"], json!(0)),
        (&["upload_reaper", "stale_after_secs"], json!(59)),
        (&["upload_reaper", "scan_interval_secs"], json!(3601)),
        (&["thread_summary_worker", "claim_timeout_secs"], json!(29)),
        (
            &["thread_summary_worker", "compression_threshold_pct"],
            json!(100),
        ),
        (&["thread_summary_worker", "max_attempts"], json!(0)),
        (&["cleanup_worker", "max_attempts"], json!(0)),
    ];
    for (path, v) in cases {
        let c = parse(with(valid(), path, v.clone()));
        let rejected = match c {
            Err(_) => true,
            Ok(c) => c.validate().is_err(),
        };
        assert!(rejected, "{path:?} = {v} must be rejected");
    }
}

#[test]
fn provider_validation() {
    let provider = |extra: serde_json::Value| {
        let mut p = json!({"kind": "openai_responses", "host": "llm.example.com", "storage_kind": "openai"});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        with(valid(), &["providers"], json!({"p": p}))
    };
    let ok = parse(provider(json!({}))).unwrap();
    ok.validate().unwrap();
    for bad in [
        json!({"host": "bad host/x"}),
        json!({"port": 0}),
        json!({"storage_kind": "azure"}),
        json!({"api_version": "bad version"}),
        json!({"rag_provider": "missing"}),
        json!({"tenant_overrides": {"t": {}}}),
    ] {
        let c = parse(provider(bad.clone()));
        let rejected = c.map_or(true, |c| c.validate().is_err());
        assert!(rejected, "{bad} must be rejected");
    }
    assert!(parse(provider(json!({"unknown": 1}))).is_err());
    assert!(parse(provider(json!({"kind": "nope"}))).is_err());
    let azure = parse(provider(
        json!({"storage_kind": "azure", "api_version": "2025-01-01-preview"}),
    ))
    .unwrap();
    azure.validate().unwrap();
}

#[test]
fn upstream_alias_defaults_to_host_and_http_port() {
    let v = with(
        valid(),
        &["providers"],
        json!({"p": {"kind": "openai_responses", "host": "127.0.0.1", "use_http": true, "storage_kind": "openai",
                      "tenant_overrides": {"t1": {"host": "other.example.com"}}}}),
    );
    let mut c = parse(v).unwrap();
    c.fill_upstream_aliases();
    let p = &c.providers["p"];
    assert_eq!(p.upstream_alias.as_deref(), Some("127.0.0.1"));
    assert_eq!(p.effective_port(), 80);
    assert_eq!(
        p.tenant_overrides["t1"].upstream_alias.as_deref(),
        Some("other.example.com")
    );
}

#[test]
fn default_alias_mirrors_oagw_derivation() {
    assert_eq!(
        default_alias("api.openai.com", 443, false),
        "api.openai.com"
    );
    assert_eq!(default_alias("llm.local", 80, true), "llm.local");
    assert_eq!(default_alias("LocalHost", 8080, true), "localhost:8080");
    assert_eq!(
        default_alias("llm.example.com", 8443, false),
        "llm.example.com:8443"
    );
    assert_eq!(default_alias("127.0.0.1", 18090, true), "127.0.0.1");
    assert_eq!(default_alias("[::1]", 18090, true), "[::1]");
}
