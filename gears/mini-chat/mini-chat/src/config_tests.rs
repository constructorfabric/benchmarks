#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// Test fixtures take owned `Value`s for readability and the assertion-heavy default
// tests are intentionally linear.
#![allow(clippy::needless_pass_by_value, clippy::cognitive_complexity)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-load-config` (and, through
//! `MiniChatConfig::load`, the parse-stage rules of
//! `cpt-cf-mini-chat-algo-gear-foundation-validate-providers`).

use serde_json::{Value, json};

use super::{
    ConfigError, DEFAULT_FILE_SEARCH_GUARD, DEFAULT_KNOWLEDGE_SEARCH_GUARD,
    DEFAULT_SUMMARY_SYSTEM_PROMPT, DEFAULT_WEB_SEARCH_GUARD, LoadedConfig, MiniChatConfig,
    ProviderKind, StorageKind,
};

const CLIENT_SECRET_SENTINEL: &str = "SENTINEL-CLIENT-SECRET-4410";
const AUTH_SENTINEL: &str = "SENTINEL-AUTH-VALUE-9921";

fn base() -> Value {
    json!({ "client_credentials": { "client_id": "mini-chat", "client_secret": CLIENT_SECRET_SENTINEL } })
}

/// Sets `path` (dot separated) in `value`, creating intermediate objects.
fn set(mut value: Value, path: &str, new: Value) -> Value {
    let mut cur = &mut value;
    for key in path.split('.') {
        cur = cur
            .as_object_mut()
            .unwrap()
            .entry(key.to_owned())
            .or_insert_with(|| json!({}));
    }
    *cur = new;
    value
}

fn load(raw: &Value) -> Result<LoadedConfig, ConfigError> {
    MiniChatConfig::load(raw)
}

fn load_ok(raw: &Value) -> LoadedConfig {
    load(raw).unwrap_or_else(|e| panic!("expected valid config, got: {e}"))
}

fn load_err(raw: &Value) -> ConfigError {
    load(raw).expect_err("expected a configuration error")
}

fn assert_no_secret(text: &str) {
    assert!(
        !text.contains(CLIENT_SECRET_SENTINEL),
        "client secret leaked: {text}"
    );
    assert!(!text.contains(AUTH_SENTINEL), "auth value leaked: {text}");
}

// ---- defaults (DESIGN Appendix B) ----------------------------------------------

#[test]
fn minimal_config_gets_every_documented_default() {
    let loaded = load_ok(&base());
    let c = &loaded.config;

    assert_eq!(c.url_prefix, "/mini-chat");
    assert_eq!(c.vendor, "constructorfabric");
    assert_eq!(c.metrics.prefix, "");

    assert_eq!(c.streaming.sse_ping_interval_seconds, 15);
    assert_eq!(c.streaming.sse_channel_capacity, 32);
    assert_eq!(c.streaming.max_output_tokens, 32768);

    let b = &c.estimation_budgets;
    assert_eq!(b.minimal_generation_floor, 50);
    assert_eq!(b.bytes_per_token_conservative, 4);
    assert_eq!(b.fixed_overhead_tokens, 100);
    assert_eq!(b.safety_margin_pct, 10);
    assert_eq!(b.image_token_budget, 1000);
    assert_eq!(b.tool_surcharge_tokens, 500);
    assert_eq!(b.web_search_surcharge_tokens, 500);
    assert_eq!(b.code_interpreter_surcharge_tokens, 1000);

    assert!((c.quota.overshoot_tolerance_factor - 1.10).abs() < f64::EPSILON);
    assert_eq!(c.quota.warning_threshold_pct, 80);
    assert_eq!(c.quota.web_search_max_calls_per_message, 2);
    assert_eq!(c.quota.web_search_daily_quota, 75);
    assert_eq!(c.quota.code_interpreter_max_calls_per_message, 10);
    assert_eq!(c.quota.code_interpreter_daily_quota, 50);

    assert_eq!(c.context.recent_messages_limit, 10);
    assert_eq!(c.context.web_search_guard, DEFAULT_WEB_SEARCH_GUARD);
    assert_eq!(c.context.file_search_guard, DEFAULT_FILE_SEARCH_GUARD);
    assert!(c.context.web_search_guard.contains("web_search"));
    assert!(!c.context.file_search_guard.is_empty());
}

#[test]
fn minimal_config_gets_rag_thumbnail_and_knowledge_defaults() {
    let c = load_ok(&base()).config;

    assert_eq!(c.rag.max_documents_per_chat, 50);
    assert_eq!(c.rag.max_total_upload_mb_per_chat, 100);
    assert_eq!(c.rag.uploaded_file_max_size_kb, 25600);
    assert_eq!(c.rag.uploaded_image_max_size_kb, 5120);
    assert_eq!(c.rag.max_images_per_message, 4);
    assert!(c.rag.allow_csv_upload);
    assert_eq!(c.rag.max_concurrent_uploads, 10);

    assert_eq!(c.thumbnail.width, 128);
    assert_eq!(c.thumbnail.height, 128);
    assert_eq!(c.thumbnail.max_bytes, 131_072);
    assert_eq!(c.thumbnail.max_pixels, 100_000_000);
    assert_eq!(c.thumbnail.max_decode_bytes, 33_554_432);

    let k = &c.knowledge_search;
    assert!(!k.enabled);
    assert_eq!(k.vector_store_id, None);
    assert_eq!(k.provider_id, None);
    assert_eq!(k.max_calls_per_message, 3);
    assert_eq!(k.top_k, 5);
    assert_eq!(k.max_chunk_chars, 2000);
    assert_eq!(k.guard, DEFAULT_KNOWLEDGE_SEARCH_GUARD);
}

#[test]
fn minimal_config_gets_worker_and_outbox_defaults() {
    let c = load_ok(&base()).config;

    assert!(c.orphan_watchdog.enabled);
    assert_eq!(c.orphan_watchdog.timeout_secs, 300);
    assert_eq!(c.orphan_watchdog.scan_interval_secs, 60);

    assert!(c.upload_reaper.enabled);
    assert_eq!(c.upload_reaper.scan_interval_secs, 60);
    assert_eq!(c.upload_reaper.stale_after_secs, 300);

    let t = &c.thread_summary_worker;
    assert!(t.enabled);
    assert_eq!(t.claim_timeout_secs, 300);
    assert_eq!(t.max_attempts, 3);
    assert_eq!(t.compression_threshold_pct, 80);
    assert_eq!(t.summary_model_id, "");
    assert_eq!(t.summary_system_prompt, DEFAULT_SUMMARY_SYSTEM_PROMPT);
    assert_eq!(t.message_content_limit, 4000);

    assert_eq!(c.cleanup_worker.max_attempts, 5);

    assert_eq!(c.outbox.queue_name, "mini-chat.usage_snapshot");
    assert_eq!(c.outbox.cleanup_queue_name, "mini-chat.attachment_cleanup");
    assert_eq!(c.outbox.chat_cleanup_queue_name, "mini-chat.chat_cleanup");
    assert_eq!(
        c.outbox.thread_summary_queue_name,
        "mini-chat.thread_summary"
    );
    assert_eq!(c.outbox.audit_queue_name, "mini-chat.audit");
    assert_eq!(c.outbox.num_partitions, 4);

    assert!(
        loaded_warnings(&base()).is_empty(),
        "defaults produce no warnings"
    );
}

fn loaded_warnings(raw: &Value) -> Vec<super::ConfigWarning> {
    load_ok(raw).warnings
}

#[test]
fn absent_providers_resolve_to_the_default_openai_entry() {
    let loaded = load_ok(&base());

    assert!(loaded.config.providers.is_none());
    assert_eq!(loaded.providers.providers.len(), 1);
    assert_eq!(loaded.providers.providers[0].id, "openai");
    assert_eq!(
        loaded.providers.providers[0].kind,
        ProviderKind::OpenaiResponses
    );
    assert_eq!(loaded.providers.targets.len(), 1);
}

#[test]
fn explicit_values_override_defaults() {
    let raw = set(
        set(
            set(base(), "url_prefix", json!("/chat")),
            "vendor",
            json!("acme"),
        ),
        "metrics.prefix",
        json!("acme_chat"),
    );

    let c = load_ok(&raw).config;

    assert_eq!(c.url_prefix, "/chat");
    assert_eq!(c.vendor, "acme");
    assert_eq!(c.metrics.prefix, "acme_chat");
}

// ---- client credentials -------------------------------------------------------

#[test]
fn client_credentials_are_required() {
    let message = load_err(&json!({})).to_string();
    assert!(message.contains("client_credentials"), "got: {message}");
}

#[test]
fn empty_client_id_or_secret_is_rejected_naming_the_key_only() {
    for (path, key) in [
        (
            "client_credentials.client_id",
            "client_credentials.client_id",
        ),
        (
            "client_credentials.client_secret",
            "client_credentials.client_secret",
        ),
    ] {
        let raw = set(base(), path, json!(""));
        let message = load_err(&raw).to_string();
        assert!(message.contains(key), "empty {key}: {message}");
        assert_no_secret(&message);
    }
}

#[test]
fn client_credentials_are_expanded_from_the_environment() {
    temp_env::with_vars(
        [
            ("MC_TEST_CLIENT_ID", Some("mini-chat-prod")),
            ("MC_TEST_CLIENT_SECRET", Some(CLIENT_SECRET_SENTINEL)),
        ],
        || {
            let raw = json!({ "client_credentials": {
                "client_id": "${MC_TEST_CLIENT_ID}", "client_secret": "${MC_TEST_CLIENT_SECRET}"
            } });

            let c = load_ok(&raw).config;

            assert_eq!(c.client_credentials.client_id, "mini-chat-prod");
            assert_eq!(
                c.client_credentials.client_secret.expose(),
                CLIENT_SECRET_SENTINEL
            );
        },
    );
}

#[test]
fn credentials_that_expand_to_empty_are_rejected() {
    temp_env::with_vars(
        [
            ("MC_TEST_EMPTY_ID", Some("")),
            ("MC_TEST_SECRET_OK", Some(CLIENT_SECRET_SENTINEL)),
        ],
        || {
            let raw = json!({ "client_credentials": {
                "client_id": "${MC_TEST_EMPTY_ID}", "client_secret": "${MC_TEST_SECRET_OK}"
            } });

            let message = load_err(&raw).to_string();

            assert!(
                message.contains("client_credentials.client_id"),
                "got: {message}"
            );
            assert_no_secret(&message);
        },
    );
}

#[test]
fn unset_variable_in_credentials_is_an_error_without_the_secret() {
    temp_env::with_var_unset("MC_TEST_MISSING_SECRET", || {
        let raw = set(
            base(),
            "client_credentials.client_secret",
            json!("${MC_TEST_MISSING_SECRET}"),
        );
        let message = load_err(&raw).to_string();
        assert!(
            message.contains("client_credentials.client_secret"),
            "got: {message}"
        );
    });
}

#[test]
fn expansion_applies_only_to_the_documented_fields() {
    temp_env::with_var("MC_TEST_VENDOR", Some("expanded"), || {
        let raw = set(base(), "vendor", json!("${MC_TEST_VENDOR}"));

        let c = load_ok(&raw).config;

        assert_eq!(
            c.vendor, "${MC_TEST_VENDOR}",
            "vendor is not an expansion site"
        );
    });
}

// ---- parse errors never echo values -------------------------------------------------

const NUMERIC_SENTINEL: &str = "8675309";

fn assert_parse_error_is_value_free(raw: &Value, sentinels: &[&str]) {
    let error = load_err(raw);
    assert!(
        matches!(error, ConfigError::Parse(_)),
        "expected a parse error, got: {error}"
    );
    for text in [
        error.to_string(),
        format!("{error:?}"),
        format!("{error:#?}"),
    ] {
        for sentinel in sentinels {
            assert!(!text.contains(sentinel), "`{sentinel}` leaked: {text}");
        }
    }
}

#[test]
fn numeric_client_secret_error_does_not_echo_the_value() {
    let raw = set(base(), "client_credentials.client_secret", json!(8_675_309));
    assert_parse_error_is_value_free(&raw, &[NUMERIC_SENTINEL]);
}

#[test]
fn boolean_client_secret_error_does_not_echo_the_value() {
    let raw = set(base(), "client_credentials.client_secret", json!(true));
    let message = load_err(&raw).to_string();
    assert!(!message.contains("true"), "got: {message}");
    assert!(
        message.contains("expected a secret string"),
        "got: {message}"
    );
}

#[test]
fn client_credentials_as_a_string_does_not_echo_the_value() {
    let raw = json!({ "client_credentials": CLIENT_SECRET_SENTINEL });
    assert_parse_error_is_value_free(&raw, &[CLIENT_SECRET_SENTINEL]);
}

#[test]
fn numeric_auth_config_value_error_does_not_echo_the_value() {
    let entry = set(
        provider_entry(),
        "auth_config",
        json!({ "secret_ref": 8_675_309 }),
    );
    let raw = set(base(), "providers.p", entry);
    assert_parse_error_is_value_free(&raw, &[NUMERIC_SENTINEL, CLIENT_SECRET_SENTINEL]);
}

#[test]
fn auth_config_as_a_string_does_not_echo_the_value() {
    let entry = set(
        provider_entry(),
        "auth_config",
        json!(format!("Bearer {AUTH_SENTINEL}")),
    );
    let raw = set(base(), "providers.p", entry);
    assert_parse_error_is_value_free(&raw, &[AUTH_SENTINEL, CLIENT_SECRET_SENTINEL]);
}

#[test]
fn auth_config_as_a_number_does_not_echo_the_value() {
    let entry = set(provider_entry(), "auth_config", json!(8_675_309));
    let raw = set(base(), "providers.p", entry);
    assert_parse_error_is_value_free(&raw, &[NUMERIC_SENTINEL]);
}

#[test]
fn tenant_override_numeric_auth_config_does_not_echo_the_value() {
    let raw = set(
        set(base(), "providers.p", provider_entry()),
        "providers.p.tenant_overrides.t1",
        json!({ "upstream_alias": "other.example.com", "auth_config": { "secret_ref": 8_675_309 } }),
    );
    assert_parse_error_is_value_free(&raw, &[NUMERIC_SENTINEL, CLIENT_SECRET_SENTINEL]);
}

#[test]
fn tenant_override_auth_config_as_a_string_does_not_echo_the_value() {
    let raw = set(
        set(base(), "providers.p", provider_entry()),
        "providers.p.tenant_overrides.t1",
        json!({ "upstream_alias": "other.example.com", "auth_config": AUTH_SENTINEL }),
    );
    assert_parse_error_is_value_free(&raw, &[AUTH_SENTINEL]);
}

#[test]
fn parse_message_redaction_strips_values_but_keeps_key_names() {
    use super::redact_parse_message;

    assert_eq!(
        redact_parse_message("invalid type: integer `12345678`, expected a string"),
        "invalid type: integer, expected a string"
    );
    assert_eq!(
        redact_parse_message(
            "invalid type: string \"a, expected b \\\" c\", expected struct Streaming"
        ),
        "invalid type: string, expected struct Streaming"
    );
    assert_eq!(
        redact_parse_message("invalid value: integer `300`, expected u16"),
        "invalid value: integer, expected u16"
    );
    assert_eq!(
        redact_parse_message("unknown field `foo`, expected one of `a`, `b`"),
        "unknown field `foo`, expected one of `a`, `b`"
    );
    assert_eq!(
        redact_parse_message("missing field `client_id`"),
        "missing field `client_id`"
    );
}

// ---- unknown / removed keys ---------------------------------------------------

#[test]
fn unknown_top_level_key_is_rejected_naming_it() {
    let raw = set(base(), "unknown_key_xyz", json!(1));
    let message = load_err(&raw).to_string();
    assert!(message.contains("unknown_key_xyz"), "got: {message}");
    assert_no_secret(&message);
}

#[test]
fn unknown_keys_are_rejected_in_every_strict_section() {
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
        let raw = set(base(), &format!("{section}.unknown_key_xyz"), json!(1));
        let message = load_err(&raw).to_string();
        assert!(
            message.contains("unknown_key_xyz"),
            "section {section}: {message}"
        );
    }
}

#[test]
fn unknown_keys_are_accepted_and_ignored_in_worker_sections() {
    for section in [
        "orphan_watchdog",
        "upload_reaper",
        "thread_summary_worker",
        "cleanup_worker",
    ] {
        let raw = set(base(), &format!("{section}.unknown_key_xyz"), json!(1));
        load_ok(&raw);
    }
}

#[test]
fn unknown_key_in_a_provider_entry_or_tenant_override_is_rejected() {
    let provider =
        json!({ "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai" });

    let entry = set(
        base(),
        "providers.p",
        set(provider.clone(), "unknown_key_xyz", json!(1)),
    );
    assert!(load_err(&entry).to_string().contains("unknown_key_xyz"));

    let override_raw = set(
        base(),
        "providers.p",
        set(
            provider,
            "tenant_overrides.t1",
            json!({ "host": "t1.example.com", "unknown_key_xyz": 1 }),
        ),
    );
    assert!(
        load_err(&override_raw)
            .to_string()
            .contains("unknown_key_xyz")
    );
}

#[test]
fn removed_keys_fail_startup() {
    let provider = json!({
        "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai",
        "supports_file_search_filters": true
    });
    let cases = [
        (
            "providers.<id>.supports_file_search_filters",
            set(base(), "providers.p", provider),
        ),
        (
            "streaming.web_search_context_size",
            set(base(), "streaming.web_search_context_size", json!("low")),
        ),
        ("mcp", set(base(), "mcp", json!({ "servers": [] }))),
    ];

    for (name, raw) in cases {
        let message = load_err(&raw).to_string();
        let key = name.rsplit('.').next().unwrap();
        assert!(
            message.contains(key),
            "removed key {name} must fail naming it, got: {message}"
        );
    }
}

// ---- ranges ---------------------------------------------------------------------

struct Range {
    path: &'static str,
    ok: Vec<Value>,
    bad: Vec<Value>,
}

fn range(path: &'static str, ok: &[Value], bad: &[Value]) -> Range {
    Range {
        path,
        ok: ok.to_vec(),
        bad: bad.to_vec(),
    }
}

#[test]
fn numeric_ranges_accept_boundaries_and_reject_just_outside() {
    let ranges = vec![
        range(
            "streaming.sse_ping_interval_seconds",
            &[json!(5), json!(15), json!(60)],
            &[json!(0), json!(4), json!(61)],
        ),
        range(
            "streaming.sse_channel_capacity",
            &[json!(16), json!(64)],
            &[json!(0), json!(15), json!(65)],
        ),
        range(
            "quota.overshoot_tolerance_factor",
            &[json!(1.0), json!(1.5)],
            &[json!(0.99), json!(1.51), json!(0.0)],
        ),
        range(
            "quota.warning_threshold_pct",
            &[json!(1), json!(99)],
            &[json!(0), json!(100)],
        ),
        range(
            "context.recent_messages_limit",
            &[json!(0), json!(100)],
            &[json!(101)],
        ),
        range(
            "rag.max_concurrent_uploads",
            &[json!(1), json!(256)],
            &[json!(0), json!(257)],
        ),
        range(
            "orphan_watchdog.timeout_secs",
            &[json!(90), json!(3600)],
            &[json!(0), json!(89), json!(3601)],
        ),
        range(
            "orphan_watchdog.scan_interval_secs",
            &[json!(1), json!(3600)],
            &[json!(0), json!(3601)],
        ),
        range(
            "upload_reaper.scan_interval_secs",
            &[json!(1), json!(3600)],
            &[json!(0), json!(3601)],
        ),
        range(
            "upload_reaper.stale_after_secs",
            &[json!(60), json!(86400)],
            &[json!(0), json!(59), json!(86401)],
        ),
        range(
            "thread_summary_worker.claim_timeout_secs",
            &[json!(30), json!(3600)],
            &[json!(0), json!(29), json!(3601)],
        ),
        range(
            "thread_summary_worker.compression_threshold_pct",
            &[json!(1), json!(99)],
            &[json!(0), json!(100)],
        ),
        range(
            "outbox.num_partitions",
            &[json!(1), json!(2), json!(4), json!(8), json!(64)],
            &[json!(0), json!(3), json!(6), json!(65), json!(128)],
        ),
    ];

    for r in ranges {
        for value in &r.ok {
            load_ok(&set(base(), r.path, value.clone()));
        }
        for value in &r.bad {
            let message = load_err(&set(base(), r.path, value.clone())).to_string();
            assert!(message.contains(r.path), "{} = {value}: {message}", r.path);
            assert_no_secret(&message);
        }
    }
}

#[test]
fn values_that_must_be_greater_than_zero_reject_zero() {
    for path in [
        "quota.web_search_max_calls_per_message",
        "quota.web_search_daily_quota",
        "quota.code_interpreter_max_calls_per_message",
        "quota.code_interpreter_daily_quota",
        "rag.max_documents_per_chat",
        "rag.max_total_upload_mb_per_chat",
        "rag.uploaded_file_max_size_kb",
        "rag.uploaded_image_max_size_kb",
        "rag.max_images_per_message",
        "thumbnail.width",
        "thumbnail.height",
        "thumbnail.max_bytes",
        "thumbnail.max_pixels",
        "thumbnail.max_decode_bytes",
        "knowledge_search.max_calls_per_message",
        "knowledge_search.top_k",
        "knowledge_search.max_chunk_chars",
        "thread_summary_worker.max_attempts",
        "cleanup_worker.max_attempts",
        "estimation_budgets.minimal_generation_floor",
    ] {
        load_ok(&set(base(), path, json!(1)));
        let message = load_err(&set(base(), path, json!(0))).to_string();
        assert!(message.contains(path), "{path} = 0: {message}");
    }
}

#[test]
fn message_content_limit_zero_means_no_truncation_and_is_valid() {
    let c = load_ok(&set(
        base(),
        "thread_summary_worker.message_content_limit",
        json!(0),
    ))
    .config;
    assert_eq!(c.thread_summary_worker.message_content_limit, 0);
}

#[test]
fn minimal_generation_floor_must_not_exceed_streaming_max_output_tokens() {
    let raw = set(base(), "streaming.max_output_tokens", json!(100));

    load_ok(&set(
        raw.clone(),
        "estimation_budgets.minimal_generation_floor",
        json!(100),
    ));
    let message = load_err(&set(
        raw,
        "estimation_budgets.minimal_generation_floor",
        json!(101),
    ))
    .to_string();

    assert!(
        message.contains("estimation_budgets.minimal_generation_floor"),
        "got: {message}"
    );
}

#[test]
fn default_floor_exceeding_a_tiny_output_cap_is_rejected() {
    let message = load_err(&set(base(), "streaming.max_output_tokens", json!(10))).to_string();
    assert!(
        message.contains("estimation_budgets.minimal_generation_floor"),
        "got: {message}"
    );
}

#[test]
fn vendor_and_queue_names_must_be_non_empty() {
    for path in [
        "vendor",
        "outbox.queue_name",
        "outbox.cleanup_queue_name",
        "outbox.chat_cleanup_queue_name",
        "outbox.thread_summary_queue_name",
        "outbox.audit_queue_name",
    ] {
        let message = load_err(&set(base(), path, json!(""))).to_string();
        assert!(message.contains(path), "{path}: {message}");
    }
}

#[test]
fn enabled_knowledge_search_requires_vector_store_and_provider() {
    let enabled = set(base(), "knowledge_search.enabled", json!(true));

    let missing_store = load_err(&set(
        enabled.clone(),
        "knowledge_search.provider_id",
        json!("openai"),
    ))
    .to_string();
    assert!(
        missing_store.contains("knowledge_search.vector_store_id"),
        "got: {missing_store}"
    );

    let missing_provider = load_err(&set(
        enabled.clone(),
        "knowledge_search.vector_store_id",
        json!("vs_1"),
    ))
    .to_string();
    assert!(
        missing_provider.contains("knowledge_search.provider_id"),
        "got: {missing_provider}"
    );

    let empty = set(
        set(
            enabled.clone(),
            "knowledge_search.vector_store_id",
            json!(""),
        ),
        "knowledge_search.provider_id",
        json!(""),
    );
    assert!(
        load_err(&empty)
            .to_string()
            .contains("knowledge_search.vector_store_id")
    );

    let complete = set(
        set(enabled, "knowledge_search.vector_store_id", json!("vs_1")),
        "knowledge_search.provider_id",
        json!("openai"),
    );
    let c = load_ok(&complete).config;
    assert!(c.knowledge_search.enabled);
    assert_eq!(c.knowledge_search.vector_store_id.as_deref(), Some("vs_1"));
}

#[test]
fn disabled_knowledge_search_does_not_require_ids() {
    load_ok(&set(base(), "knowledge_search.enabled", json!(false)));
}

// ---- warnings ---------------------------------------------------------------------

#[test]
fn deprecated_budget_fields_warn_only_when_set_to_a_non_default_value() {
    let raw = set(
        set(base(), "estimation_budgets.safety_margin_pct", json!(25)),
        "estimation_budgets.image_token_budget",
        json!(1000),
    );

    let loaded = load_ok(&raw);

    let keys: Vec<&str> = loaded.warnings.iter().map(|w| w.key.as_str()).collect();
    assert_eq!(keys, ["estimation_budgets.safety_margin_pct"]);
    assert_eq!(
        loaded.config.estimation_budgets.safety_margin_pct, 25,
        "parsed, not rejected"
    );
}

#[test]
fn deprecated_budget_fields_are_not_range_validated() {
    let raw = set(
        base(),
        "estimation_budgets.bytes_per_token_conservative",
        json!(0),
    );
    let loaded = load_ok(&raw);
    assert_eq!(loaded.warnings.len(), 1);
}

#[test]
fn minimal_generation_floor_is_not_deprecated() {
    let loaded = load_ok(&set(
        base(),
        "estimation_budgets.minimal_generation_floor",
        json!(64),
    ));
    assert!(loaded.warnings.is_empty());
}

#[test]
fn every_deprecated_budget_field_has_its_own_warning() {
    let mut raw = base();
    for (field, value) in [
        ("bytes_per_token_conservative", 3),
        ("fixed_overhead_tokens", 1),
        ("safety_margin_pct", 1),
        ("image_token_budget", 1),
        ("tool_surcharge_tokens", 1),
        ("web_search_surcharge_tokens", 1),
        ("code_interpreter_surcharge_tokens", 1),
    ] {
        raw = set(raw, &format!("estimation_budgets.{field}"), json!(value));
    }

    assert_eq!(load_ok(&raw).warnings.len(), 7);
}

#[test]
fn deprecated_worker_fields_warn_when_they_have_no_effect() {
    let raw = set(
        set(
            set(
                set(
                    set(
                        set(base(), "cleanup_worker.enabled", json!(false)),
                        "cleanup_worker.poll_interval_secs",
                        json!(61),
                    ),
                    "cleanup_worker.reconcile_interval_secs",
                    json!(301),
                ),
                "cleanup_worker.stale_in_progress_timeout_secs",
                json!(901),
            ),
            "cleanup_worker.batch_size",
            json!(33),
        ),
        "thread_summary_worker.reconcile_interval_secs",
        json!(61),
    );

    let keys: Vec<String> = load_ok(&raw).warnings.into_iter().map(|w| w.key).collect();

    for want in [
        "cleanup_worker.enabled",
        "cleanup_worker.poll_interval_secs",
        "cleanup_worker.reconcile_interval_secs",
        "cleanup_worker.stale_in_progress_timeout_secs",
        "cleanup_worker.batch_size",
        "thread_summary_worker.reconcile_interval_secs",
    ] {
        assert!(
            keys.iter().any(|k| k == want),
            "missing warning for {want}: {keys:?}"
        );
    }
    assert_eq!(keys.len(), 6);
}

#[test]
fn deprecated_worker_fields_at_their_defaults_do_not_warn() {
    let raw = set(
        set(base(), "cleanup_worker.enabled", json!(true)),
        "cleanup_worker.poll_interval_secs",
        json!(60),
    );
    assert!(load_ok(&raw).warnings.is_empty());
}

// ---- providers through load -------------------------------------------------------

fn provider_entry() -> Value {
    json!({ "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai" })
}

#[test]
fn provider_required_keys_are_enforced_at_parse_time() {
    for missing in ["kind", "host", "storage_kind"] {
        let mut entry = provider_entry();
        entry.as_object_mut().unwrap().remove(missing);

        let message = load_err(&set(base(), "providers.p", entry)).to_string();

        assert!(message.contains(missing), "missing `{missing}`: {message}");
    }
}

#[test]
fn unknown_provider_kind_and_storage_kind_are_rejected() {
    for (key, value) in [("kind", "gemini"), ("storage_kind", "s3")] {
        let mut entry = provider_entry();
        entry[key] = json!(value);
        let message = load_err(&set(base(), "providers.p", entry)).to_string();
        assert!(message.contains(value), "{key}={value}: {message}");
    }
}

#[test]
fn providers_are_validated_as_part_of_load() {
    let entry = set(provider_entry(), "port", json!(0));

    let message = load_err(&set(base(), "providers.p", entry)).to_string();

    assert!(message.contains("providers.p.port"), "got: {message}");
}

#[test]
fn loaded_providers_resolve_one_registry_entry_per_configured_provider() {
    let raw = set(
        set(
            base(),
            "providers.zeta",
            set(provider_entry(), "host", json!("zeta.example.com")),
        ),
        "providers.alpha",
        set(provider_entry(), "host", json!("alpha.example.com")),
    );

    let loaded = load_ok(&raw);

    let mut ids: Vec<&str> = loaded
        .providers
        .providers
        .iter()
        .map(|p| p.id.as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, ["alpha", "zeta"]);
    assert_eq!(loaded.providers.targets.len(), 2);
    assert!(
        loaded
            .providers
            .providers
            .iter()
            .all(|p| p.storage_kind == StorageKind::Openai)
    );
}

#[test]
fn provider_env_expansion_happens_during_load() {
    temp_env::with_vars(
        [
            ("MC_TEST_LOAD_HOST", Some("expanded.example.com")),
            ("MC_TEST_LOAD_KEY", Some(AUTH_SENTINEL)),
        ],
        || {
            let entry = set(
                set(provider_entry(), "host", json!("${MC_TEST_LOAD_HOST}")),
                "auth_config",
                json!({ "header": "api-key", "secret_ref": "${MC_TEST_LOAD_KEY}" }),
            );

            let loaded = load_ok(&set(base(), "providers.p", entry));

            assert_eq!(loaded.providers.providers[0].host, "expanded.example.com");
            assert_eq!(
                loaded.providers.providers[0].auth_config.get("secret_ref"),
                Some(AUTH_SENTINEL)
            );
        },
    );
}

#[test]
fn alias_conflict_is_reported_by_load() {
    let a = set(
        provider_entry(),
        "auth_config",
        json!({ "secret_ref": "k1" }),
    );
    let b = set(
        provider_entry(),
        "auth_config",
        json!({ "secret_ref": "k2" }),
    );
    let raw = set(set(base(), "providers.a", a), "providers.b", b);

    let error = load_err(&raw);

    assert!(
        matches!(error, ConfigError::AliasConflict { ref alias, .. } if alias == "api.example.com"),
        "got: {error}"
    );
}

// ---- secrets ----------------------------------------------------------------------

#[test]
fn secrets_never_appear_in_debug_output_of_the_loaded_config() {
    let entry = set(
        provider_entry(),
        "auth_config",
        json!({ "header": "api-key", "secret_ref": AUTH_SENTINEL }),
    );
    let loaded = load_ok(&set(base(), "providers.p", entry));

    for text in [
        format!("{loaded:?}"),
        format!("{loaded:#?}"),
        format!("{:?}", loaded.config),
    ] {
        assert_no_secret(&text);
    }
    assert_eq!(
        loaded.config.client_credentials.client_secret.expose(),
        CLIENT_SECRET_SENTINEL
    );
}

#[test]
fn secrets_never_appear_in_validation_errors_or_warnings() {
    let entry = set(
        provider_entry(),
        "auth_config",
        json!({ "secret_ref": AUTH_SENTINEL }),
    );
    let with_provider = set(base(), "providers.p", entry);

    let failures = [
        set(with_provider.clone(), "unknown_key_xyz", json!(1)),
        set(
            with_provider.clone(),
            "streaming.sse_ping_interval_seconds",
            json!(1),
        ),
        set(with_provider.clone(), "providers.p.port", json!(0)),
        set(with_provider.clone(), "providers.p.host", json!("bad/host")),
        set(
            with_provider.clone(),
            "providers.p.rag_provider",
            json!("nope"),
        ),
        set(
            with_provider.clone(),
            "providers.p.tenant_overrides.t1",
            json!({ "upstream_alias": "api.example.com", "auth_config": { "secret_ref": "other" } }),
        ),
        set(
            with_provider.clone(),
            "client_credentials.client_id",
            json!(""),
        ),
    ];
    for raw in failures {
        let error = load_err(&raw);
        assert_no_secret(&error.to_string());
        assert_no_secret(&format!("{error:?}"));
    }

    let warned = set(
        with_provider,
        "estimation_budgets.safety_margin_pct",
        json!(25),
    );
    let loaded = load_ok(&warned);
    assert!(!loaded.warnings.is_empty());
    assert_no_secret(&format!("{:?}", loaded.warnings));
}

// ---- config/mini-chat.yaml ------------------------------------------------------------

/// The `gears.mini-chat.config` block of `config/mini-chat.yaml` (comments dropped).
const MINI_CHAT_YAML: &str = r#"
vendor: "constructorfabric"
orphan_watchdog:
  scan_interval_secs: 10
  timeout_secs: 90
client_credentials:
  client_id: "mini-chat"
  client_secret: "mini-chat-dev-secret"
thread_summary_worker:
  enabled: true
  summary_model_id: "gpt-4.1-mini"
  compression_threshold_pct: 80
  summary_system_prompt: "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation."
providers:
  azure_openai:
    kind: openai_responses
    storage_kind: azure
    host: "${AZURE_OPENAI_API_HOST}"
    api_path: "/openai/v1/responses"
    api_version: "2025-03-01-preview"
    auth_plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    auth_config:
      header: "api-key"
      prefix: ""
      secret_ref: "azure-openai-key"
"#;

fn yaml_value() -> Value {
    serde_saphyr::from_str(MINI_CHAT_YAML).expect("fixture is valid YAML")
}

#[test]
fn yaml_fixture_parses_into_a_json_value() {
    let value = yaml_value();
    assert_eq!(
        value["providers"]["azure_openai"]["kind"],
        json!("openai_responses")
    );
    assert_eq!(value["orphan_watchdog"]["scan_interval_secs"], json!(10));
}

#[test]
fn config_yaml_block_loads_with_the_azure_host_from_the_environment() {
    temp_env::with_var(
        "AZURE_OPENAI_API_HOST",
        Some("contoso.openai.azure.com"),
        || {
            let loaded = load_ok(&yaml_value());

            let c = &loaded.config;
            assert_eq!(c.vendor, "constructorfabric");
            assert_eq!(c.orphan_watchdog.scan_interval_secs, 10);
            assert_eq!(c.orphan_watchdog.timeout_secs, 90);
            assert_eq!(c.client_credentials.client_id, "mini-chat");
            assert_eq!(
                c.client_credentials.client_secret.expose(),
                "mini-chat-dev-secret"
            );
            assert_eq!(c.thread_summary_worker.summary_model_id, "gpt-4.1-mini");
            assert_eq!(
                c.thread_summary_worker.summary_system_prompt,
                DEFAULT_SUMMARY_SYSTEM_PROMPT
            );

            assert_eq!(loaded.providers.providers.len(), 1);
            let p = &loaded.providers.providers[0];
            assert_eq!(p.id, "azure_openai");
            assert_eq!(p.kind, ProviderKind::OpenaiResponses);
            assert_eq!(p.storage_kind, StorageKind::Azure);
            assert_eq!(p.host, "contoso.openai.azure.com");
            assert_eq!(p.upstream_alias, "contoso.openai.azure.com");
            assert_eq!(p.port, 443);
            assert_eq!(p.api_path, "/openai/v1/responses");
            assert_eq!(p.api_version.as_deref(), Some("2025-03-01-preview"));
            assert_eq!(
                p.auth_plugin_type.as_deref(),
                Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
            );
            assert_eq!(p.auth_config.get("header"), Some("api-key"));
            assert_eq!(p.auth_config.get("prefix"), Some(""));
            assert_eq!(p.auth_config.get("secret_ref"), Some("azure-openai-key"));
            assert_eq!(p.storage_backend, "azure_openai");
            assert!(loaded.warnings.is_empty());
        },
    );
}

#[test]
fn config_yaml_block_fails_when_the_azure_host_variable_is_unset() {
    temp_env::with_var_unset("AZURE_OPENAI_API_HOST", || {
        let message = load_err(&yaml_value()).to_string();
        assert!(
            message.contains("providers.azure_openai.host"),
            "got: {message}"
        );
    });
}
