//! Range / cross-field validation of the gear configuration (DESIGN Appendix
//! B.1-B.9): each documented range is accepted at its boundaries and rejected
//! one step outside, "> 0" keys reject zero, and the provider rules (host
//! characters, `azure` needs `api_version`, `rag_provider` references,
//! tenant overrides) hold.
//!
//! Every case goes through `parse + validate()` so the tests do not care which
//! of the two rejects the value (B.1: "Every section is validated at gear init").

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat::config::MiniChatConfig;

const CREDENTIALS: &str = "client_credentials:\n  client_id: mini-chat\n  client_secret: s3cr3t\n";

fn load(extra: &str) -> Result<MiniChatConfig, String> {
    let yaml = format!("{CREDENTIALS}{extra}");
    let cfg: MiniChatConfig = serde_saphyr::from_str(&yaml).map_err(|e| e.to_string())?;
    cfg.validate()?;
    Ok(cfg)
}

/// One `section.key: value` override rendered as YAML.
fn set(section: &str, key: &str, value: &str) -> String {
    format!("{section}:\n  {key}: {value}\n")
}

#[track_caller]
fn assert_accepts(section: &str, key: &str, value: &str) {
    if let Err(e) = load(&set(section, key, value)) {
        panic!("{section}.{key}: {value} should be accepted, got: {e}");
    }
}

#[track_caller]
fn assert_rejects(section: &str, key: &str, value: &str) {
    assert!(
        load(&set(section, key, value)).is_err(),
        "{section}.{key}: {value} should be rejected"
    );
}

/// Inclusive integer range: both ends accepted, both neighbours rejected.
#[track_caller]
fn assert_range(section: &str, key: &str, lo: u64, hi: u64) {
    assert_accepts(section, key, &lo.to_string());
    assert_accepts(section, key, &hi.to_string());
    if lo > 0 {
        assert_rejects(section, key, &(lo - 1).to_string());
    }
    assert_rejects(section, key, &(hi + 1).to_string());
}

/// `> 0` keys: zero rejected, one accepted.
#[track_caller]
fn assert_positive(section: &str, key: &str) {
    assert_rejects(section, key, "0");
    assert_accepts(section, key, "1");
}

// ── top level ────────────────────────────────────────────────────────────────

#[test]
fn vendor_must_be_non_empty() {
    assert!(load("vendor: \"\"\n").is_err());
    assert!(load("vendor: \"   \"\n").is_err());
    assert!(load("vendor: acme\n").is_ok());
}

#[test]
fn url_prefix_must_be_empty_or_a_clean_absolute_path() {
    for ok in ["\"\"", "/mini-chat", "/api/mini-chat", "/chat"] {
        assert!(load(&format!("url_prefix: {ok}\n")).is_ok(), "{ok}");
    }
    for bad in [
        "api/mini-chat",
        "\"/mini-chat/\"",
        "\"/\"",
        "\"/mini-{chat}\"",
        "\"/mini-chat}\"",
        "\"/mini?x=1\"",
        "\"/mini#frag\"",
        "\"/mini chat\"",
        "\"/mini\\tchat\"",
        "\" /mini\"",
        "\"/*\"",
        "\"/:id\"",
        "\"/a/:x\"",
        "\"/a/*x\"",
        "\"/x/:\"",
        "\"/a//b\"",
        "\"/a/../b\"",
        "\"/a%20b\"",
    ] {
        let err = load(&format!("url_prefix: {bad}\n")).expect_err(bad);
        assert!(err.contains("url_prefix"), "{bad}: {err}");
    }
}

#[test]
fn client_credentials_must_be_non_empty() {
    for yaml in [
        "client_credentials:\n  client_id: \"\"\n  client_secret: s\n",
        "client_credentials:\n  client_id: a\n  client_secret: \"\"\n",
        "client_credentials:\n  client_id: a\n",
        "client_credentials:\n  client_secret: s\n",
    ] {
        let cfg: Result<MiniChatConfig, _> = serde_saphyr::from_str(yaml);
        let rejected = match cfg {
            Err(_) => true,
            Ok(c) => c.validate().is_err(),
        };
        assert!(
            rejected,
            "credentials must be complete and non-empty: {yaml}"
        );
    }
}

// ── streaming (B.4) ──────────────────────────────────────────────────────────

#[test]
fn sse_ping_interval_range_is_5_to_60() {
    assert_range("streaming", "sse_ping_interval_seconds", 5, 60);
}

#[test]
fn sse_channel_capacity_range_is_16_to_64() {
    assert_range("streaming", "sse_channel_capacity", 16, 64);
}

#[test]
fn streaming_max_output_tokens_cannot_be_below_the_generation_floor() {
    // floor defaults to 50
    assert_accepts("streaming", "max_output_tokens", "50");
    assert_rejects("streaming", "max_output_tokens", "49");
}

// ── estimation budgets (B.5.2) ───────────────────────────────────────────────

#[test]
fn minimal_generation_floor_must_be_positive() {
    assert_positive("estimation_budgets", "minimal_generation_floor");
}

#[test]
fn minimal_generation_floor_cannot_exceed_streaming_max_output_tokens() {
    let ok = "streaming:\n  max_output_tokens: 100\nestimation_budgets:\n  minimal_generation_floor: 100\n";
    assert!(load(ok).is_ok());
    let bad = "streaming:\n  max_output_tokens: 100\nestimation_budgets:\n  minimal_generation_floor: 101\n";
    assert!(load(bad).is_err());
    // against the default cap of 32768
    assert_accepts("estimation_budgets", "minimal_generation_floor", "32768");
    assert_rejects("estimation_budgets", "minimal_generation_floor", "32769");
}

// ── quota (B.5.3, B.6) ───────────────────────────────────────────────────────

#[test]
fn overshoot_tolerance_factor_range_is_1_0_to_1_5() {
    for ok in ["1.0", "1.1", "1.5"] {
        assert_accepts("quota", "overshoot_tolerance_factor", ok);
    }
    for bad in ["0.99", "1.51", "0.0", "2.0"] {
        assert_rejects("quota", "overshoot_tolerance_factor", bad);
    }
}

#[test]
fn warning_threshold_pct_range_is_1_to_99() {
    assert_range("quota", "warning_threshold_pct", 1, 99);
}

#[test]
fn quota_call_limits_must_be_positive() {
    for key in [
        "web_search_max_calls_per_message",
        "web_search_daily_quota",
        "code_interpreter_max_calls_per_message",
        "code_interpreter_daily_quota",
    ] {
        assert_positive("quota", key);
    }
}

// ── outbox (B.9.3) ───────────────────────────────────────────────────────────

#[test]
fn outbox_num_partitions_must_be_a_power_of_two_between_1_and_64() {
    for ok in [1, 2, 4, 8, 16, 32, 64] {
        assert_accepts("outbox", "num_partitions", &ok.to_string());
    }
    for bad in [0, 3, 5, 6, 7, 12, 63, 65, 100, 128] {
        assert_rejects("outbox", "num_partitions", &bad.to_string());
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
        assert_rejects("outbox", key, "\"\"");
        assert_accepts("outbox", key, "custom.queue");
    }
}

// ── context (B.5.1) ──────────────────────────────────────────────────────────

#[test]
fn recent_messages_limit_range_is_0_to_100() {
    assert_range("context", "recent_messages_limit", 0, 100);
}

// ── rag / uploads (B.7, B.8) ─────────────────────────────────────────────────

#[test]
fn rag_limits_must_be_positive() {
    for key in [
        "max_documents_per_chat",
        "max_total_upload_mb_per_chat",
        "uploaded_file_max_size_kb",
        "uploaded_image_max_size_kb",
        "max_images_per_message",
    ] {
        assert_positive("rag", key);
    }
}

#[test]
fn max_concurrent_uploads_range_is_1_to_256() {
    assert_range("rag", "max_concurrent_uploads", 1, 256);
}

#[test]
fn thumbnail_parameters_must_be_positive() {
    for key in [
        "width",
        "height",
        "max_bytes",
        "max_pixels",
        "max_decode_bytes",
    ] {
        assert_positive("thumbnail", key);
    }
}

// ── workers (B.9) ────────────────────────────────────────────────────────────

#[test]
fn orphan_watchdog_timeout_range_is_90_to_3600() {
    assert_range("orphan_watchdog", "timeout_secs", 90, 3600);
}

#[test]
fn orphan_watchdog_scan_interval_range_is_1_to_3600() {
    assert_range("orphan_watchdog", "scan_interval_secs", 1, 3600);
}

#[test]
fn upload_reaper_scan_interval_range_is_1_to_3600() {
    assert_range("upload_reaper", "scan_interval_secs", 1, 3600);
}

#[test]
fn upload_reaper_stale_after_range_is_60_to_86400() {
    assert_range("upload_reaper", "stale_after_secs", 60, 86_400);
}

#[test]
fn thread_summary_claim_timeout_range_is_30_to_3600() {
    assert_range("thread_summary_worker", "claim_timeout_secs", 30, 3600);
}

#[test]
fn thread_summary_max_attempts_must_be_positive() {
    assert_positive("thread_summary_worker", "max_attempts");
}

#[test]
fn thread_summary_compression_threshold_range_is_1_to_99() {
    assert_range("thread_summary_worker", "compression_threshold_pct", 1, 99);
}

#[test]
fn cleanup_worker_max_attempts_must_be_positive() {
    assert_positive("cleanup_worker", "max_attempts");
}

// ── knowledge search (B.7) ───────────────────────────────────────────────────

#[test]
fn enabled_knowledge_search_requires_vector_store_and_provider() {
    let base = "knowledge_search:\n  enabled: true\n";
    assert!(load(base).is_err(), "no ids");
    assert!(
        load(&format!("{base}  vector_store_id: vs_1\n")).is_err(),
        "no provider_id"
    );
    assert!(
        load(&format!("{base}  provider_id: openai\n")).is_err(),
        "no vector_store_id"
    );
    assert!(
        load(&format!(
            "{base}  vector_store_id: vs_1\n  provider_id: openai\n"
        ))
        .is_ok()
    );
}

#[test]
fn disabled_knowledge_search_needs_no_ids() {
    assert!(load("knowledge_search:\n  enabled: false\n").is_ok());
}

#[test]
fn enabled_knowledge_search_numeric_limits_must_be_positive() {
    for key in ["max_calls_per_message", "top_k", "max_chunk_chars"] {
        let bad = format!(
            "knowledge_search:\n  enabled: true\n  vector_store_id: vs_1\n  provider_id: openai\n  {key}: 0\n"
        );
        assert!(
            load(&bad).is_err(),
            "knowledge_search.{key}: 0 must be rejected"
        );
        let ok = format!(
            "knowledge_search:\n  enabled: true\n  vector_store_id: vs_1\n  provider_id: openai\n  {key}: 1\n"
        );
        assert!(
            load(&ok).is_ok(),
            "knowledge_search.{key}: 1 must be accepted"
        );
    }
}

// ── providers (B.1 provider table) ───────────────────────────────────────────

fn provider(body: &str) -> String {
    let indented = body
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("providers:\n  p:\n{indented}\n")
}

fn openai_provider(extra: &str) -> String {
    provider(&format!(
        "kind: openai_responses\nstorage_kind: openai\n{extra}"
    ))
}

#[test]
fn provider_host_is_required_and_non_empty() {
    assert!(load(&provider("kind: openai_responses\nstorage_kind: openai\n")).is_err());
    assert!(load(&openai_provider("host: \"\"\n")).is_err());
    assert!(load(&openai_provider("host: \"  \"\n")).is_err());
    assert!(load(&openai_provider("host: api.example.com\n")).is_ok());
}

#[test]
fn provider_kind_and_storage_kind_are_required() {
    assert!(load(&provider("host: h.example\nstorage_kind: openai\n")).is_err());
    assert!(load(&provider("kind: openai_responses\nhost: h.example\n")).is_err());
}

#[test]
fn provider_kind_accepts_exactly_the_four_documented_values() {
    for kind in [
        "openai_responses",
        "openai_chat_completions",
        "vllm_responses",
        "anthropic_messages",
    ] {
        let yaml = provider(&format!(
            "kind: {kind}\nhost: h.example\nstorage_kind: openai\n"
        ));
        assert!(load(&yaml).is_ok(), "kind {kind}");
    }
    let bad = provider("kind: gemini\nhost: h.example\nstorage_kind: openai\n");
    assert!(load(&bad).is_err());
}

#[test]
fn provider_host_character_set_is_restricted() {
    // The host becomes the OAGW alias in `/{alias}/...`.
    for ok in [
        "api.openai.com",
        "my-host_1.internal",
        "127.0.0.1",
        "host.example:8443",
        "[::1]",
    ] {
        assert!(
            load(&openai_provider(&format!("host: \"{ok}\"\n"))).is_ok(),
            "host `{ok}` should be accepted"
        );
    }
    for bad in [
        "api.example.com/v1",
        "host/",
        "host?x=1",
        "host#frag",
        "user@host",
        "has space.example",
        "${UNEXPANDED_VAR}",
    ] {
        assert!(
            load(&openai_provider(&format!("host: \"{bad}\"\n"))).is_err(),
            "host `{bad}` should be rejected"
        );
    }
}

#[test]
fn provider_port_zero_is_rejected() {
    assert!(load(&openai_provider("host: h.example\nport: 0\n")).is_err());
    assert!(load(&openai_provider("host: h.example\nport: 1\n")).is_ok());
    assert!(load(&openai_provider("host: h.example\nport: 65535\n")).is_ok());
    assert!(load(&openai_provider("host: h.example\nport: 65536\n")).is_err());
}

#[test]
fn azure_storage_requires_a_valid_api_version() {
    let azure = |extra: &str| {
        provider(&format!(
            "kind: openai_responses\nhost: acme.openai.azure.com\nstorage_kind: azure\n{extra}"
        ))
    };
    assert!(load(&azure("")).is_err(), "azure without api_version");
    assert!(load(&azure("api_version: \"\"\n")).is_err());
    assert!(load(&azure("api_version: \"   \"\n")).is_err());
    assert!(load(&azure("api_version: 2025-03-01-preview\n")).is_ok());
    assert!(load(&azure("api_version: \"2024-10-21\"\n")).is_ok());
    // sent unencoded as `?api-version=...`: only letters, digits, `.` and `-`
    assert!(load(&azure("api_version: \"2025 03\"\n")).is_err());
    assert!(load(&azure("api_version: \"2025&x=1\"\n")).is_err());
    assert!(load(&azure("api_version: \"a/b\"\n")).is_err());
}

#[test]
fn openai_storage_does_not_need_an_api_version() {
    assert!(load(&openai_provider("host: h.example\n")).is_ok());
}

#[test]
fn rag_provider_must_reference_an_existing_entry() {
    let two = |rag: &str| {
        format!(
            "providers:\n  a:\n    kind: anthropic_messages\n    host: a.example\n    storage_kind: openai\n    rag_provider: {rag}\n  b:\n    kind: openai_responses\n    host: b.example\n    storage_kind: openai\n"
        )
    };
    assert!(load(&two("b")).is_ok());
    assert!(load(&two("missing")).is_err());
}

#[test]
fn tenant_override_must_set_host_or_upstream_alias() {
    let with_override = |body: &str| {
        format!(
            "providers:\n  p:\n    kind: openai_responses\n    host: h.example\n    storage_kind: openai\n    tenant_overrides:\n      \"00000000-0000-0000-0000-0000000000aa\":\n{body}"
        )
    };
    assert!(load(&with_override("        host: t.example\n")).is_ok());
    assert!(load(&with_override("        upstream_alias: tenant-alias\n")).is_ok());
    assert!(
        load(&with_override(
            "        auth_config:\n          secret_ref: only-auth\n"
        ))
        .is_err(),
        "an override with neither host nor upstream_alias is rejected"
    );
}

#[test]
fn tenant_override_host_has_the_same_character_check() {
    let with_host = |host: &str| {
        format!(
            "providers:\n  p:\n    kind: openai_responses\n    host: h.example\n    storage_kind: openai\n    tenant_overrides:\n      \"00000000-0000-0000-0000-0000000000aa\":\n        host: \"{host}\"\n"
        )
    };
    assert!(load(&with_host("tenant.example")).is_ok());
    assert!(load(&with_host("tenant.example/path")).is_err());
    assert!(load(&with_host("a@b")).is_err());
}

#[test]
fn storage_kind_accepts_only_openai_or_azure() {
    for ok in ["openai", "azure"] {
        let extra = if ok == "azure" {
            "api_version: 2025-03-01\n"
        } else {
            ""
        };
        let yaml = provider(&format!(
            "kind: openai_responses\nhost: h.example\nstorage_kind: {ok}\n{extra}"
        ));
        assert!(load(&yaml).is_ok(), "storage_kind {ok}");
    }
    let bad = provider("kind: openai_responses\nhost: h.example\nstorage_kind: s3\n");
    assert!(load(&bad).is_err());
}
