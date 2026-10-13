//! Validation steps of `cpt-cf-mini-chat-algo-gear-foundation-load-config`.
//!
//! Every function names only the key path and the violated rule in its errors;
//! no value is ever echoed, so secrets cannot reach a log or an error `detail`.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-mcp-absent:p1

use serde_json::Value;
use toolkit::var_expand::expand_env_vars;

use super::{ClientCredentials, ConfigError, ConfigWarning, MiniChatConfig, SecretValue};

/// Collects startup warnings and logs each of them once.
#[derive(Debug, Default)]
pub(super) struct Warnings(Vec<ConfigWarning>);

impl Warnings {
    pub(super) fn into_vec(self) -> Vec<ConfigWarning> {
        self.0
    }

    fn push(&mut self, key: &str, message: &str) {
        tracing::warn!(config_key = key, "{message}");
        self.0.push(ConfigWarning {
            key: key.to_owned(),
            message: message.to_owned(),
        });
    }

    /// Warns that a deprecated key was set to a value other than its default.
    fn deprecated_if_changed<T: PartialEq>(&mut self, key: &str, value: &T, default: &T) {
        if value != default {
            self.push(key, "deprecated key has no effect and will be removed");
        }
    }
}

fn to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn in_range(key: &str, value: u64, min: u64, max: u64) -> Result<(), ConfigError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError::invalid(
            key,
            format!("must be between {min} and {max}"),
        ))
    }
}

fn positive(key: &str, value: u64) -> Result<(), ConfigError> {
    if value > 0 {
        Ok(())
    } else {
        Err(ConfigError::invalid(key, "must be greater than 0"))
    }
}

fn non_empty(key: &str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty() {
        Err(ConfigError::invalid(key, "must not be empty"))
    } else {
        Ok(())
    }
}

/// Rejects the removed keys with their key path (they would also fail as unknown keys).
pub(super) fn reject_removed_keys(raw: &Value) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-removed-keys
    let removed = |key: &str| ConfigError::invalid(key, "removed key is no longer supported");
    if raw.get("mcp").is_some() {
        return Err(removed("mcp"));
    }
    if raw
        .get("streaming")
        .and_then(|streaming| streaming.get("web_search_context_size"))
        .is_some()
    {
        return Err(removed("streaming.web_search_context_size"));
    }
    if let Some(providers) = raw.get("providers").and_then(Value::as_object) {
        for (id, entry) in providers {
            if entry.get("supports_file_search_filters").is_some() {
                return Err(removed(&format!(
                    "providers.{id}.supports_file_search_filters"
                )));
            }
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-removed-keys
    Ok(())
}

/// Applies the top-level rules: `vendor` must be non-empty.
pub(super) fn validate_top_level(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-top-defaults
    non_empty("vendor", &config.vendor)?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-top-defaults
    Ok(())
}

/// Expands `${VAR}` in one credential and requires a non-empty result.
fn expand_credential(key: &str, raw: &str) -> Result<String, ConfigError> {
    let expanded = expand_env_vars(raw).map_err(|_| {
        ConfigError::invalid(key, "references an environment variable that is not set")
    })?;
    non_empty(key, &expanded)?;
    Ok(expanded)
}

/// Expands and checks `client_credentials`; `client_secret` is never reported.
pub(super) fn expand_client_credentials(
    credentials: &mut ClientCredentials,
) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-client-credentials
    credentials.client_id =
        expand_credential("client_credentials.client_id", &credentials.client_id)?;
    let secret = expand_credential(
        "client_credentials.client_secret",
        credentials.client_secret.expose(),
    )?;
    credentials.client_secret = SecretValue::new(secret);
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-client-credentials
    Ok(())
}

fn validate_streaming(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-streaming
    let streaming = &config.streaming;
    in_range(
        "streaming.sse_ping_interval_seconds",
        u64::from(streaming.sse_ping_interval_seconds),
        5,
        60,
    )?;
    in_range(
        "streaming.sse_channel_capacity",
        u64::from(streaming.sse_channel_capacity),
        16,
        64,
    )?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-streaming
    Ok(())
}

fn validate_floor(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-floor
    let key = "estimation_budgets.minimal_generation_floor";
    let floor = config.estimation_budgets.minimal_generation_floor;
    positive(key, u64::from(floor))?;
    let cap = config.streaming.max_output_tokens;
    if floor > cap {
        return Err(ConfigError::invalid(
            key,
            format!("must not exceed streaming.max_output_tokens ({cap})"),
        ));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-floor
    Ok(())
}

fn warn_deprecated_budgets(config: &MiniChatConfig, warnings: &mut Warnings) {
    let b = &config.estimation_budgets;
    let fields = [
        (
            "bytes_per_token_conservative",
            b.bytes_per_token_conservative,
            4,
        ),
        ("fixed_overhead_tokens", b.fixed_overhead_tokens, 100),
        ("safety_margin_pct", b.safety_margin_pct, 10),
        ("image_token_budget", b.image_token_budget, 1000),
        ("tool_surcharge_tokens", b.tool_surcharge_tokens, 500),
        (
            "web_search_surcharge_tokens",
            b.web_search_surcharge_tokens,
            500,
        ),
        (
            "code_interpreter_surcharge_tokens",
            b.code_interpreter_surcharge_tokens,
            1000,
        ),
    ];
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-for-each-deprecated-budget
    for (field, value, default) in fields {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-warn-deprecated-budget
        warnings.deprecated_if_changed(&format!("estimation_budgets.{field}"), &value, &default);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-warn-deprecated-budget
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-for-each-deprecated-budget
}

fn validate_quota(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-quota
    let quota = &config.quota;
    if !(1.0..=1.5).contains(&quota.overshoot_tolerance_factor) {
        return Err(ConfigError::invalid(
            "quota.overshoot_tolerance_factor",
            "must be between 1.0 and 1.5",
        ));
    }
    in_range(
        "quota.warning_threshold_pct",
        u64::from(quota.warning_threshold_pct),
        1,
        99,
    )?;
    for (key, value) in [
        (
            "quota.web_search_max_calls_per_message",
            quota.web_search_max_calls_per_message,
        ),
        ("quota.web_search_daily_quota", quota.web_search_daily_quota),
        (
            "quota.code_interpreter_max_calls_per_message",
            quota.code_interpreter_max_calls_per_message,
        ),
        (
            "quota.code_interpreter_daily_quota",
            quota.code_interpreter_daily_quota,
        ),
    ] {
        positive(key, u64::from(value))?;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-quota
    Ok(())
}

fn validate_context(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-context
    in_range(
        "context.recent_messages_limit",
        u64::from(config.context.recent_messages_limit),
        0,
        100,
    )?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-context
    Ok(())
}

fn validate_rag(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-rag
    let rag = &config.rag;
    for (key, value) in [
        ("rag.max_documents_per_chat", rag.max_documents_per_chat),
        (
            "rag.max_total_upload_mb_per_chat",
            rag.max_total_upload_mb_per_chat,
        ),
        (
            "rag.uploaded_file_max_size_kb",
            rag.uploaded_file_max_size_kb,
        ),
        (
            "rag.uploaded_image_max_size_kb",
            rag.uploaded_image_max_size_kb,
        ),
        ("rag.max_images_per_message", rag.max_images_per_message),
    ] {
        positive(key, u64::from(value))?;
    }
    in_range(
        "rag.max_concurrent_uploads",
        u64::from(rag.max_concurrent_uploads),
        1,
        256,
    )?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-rag
    Ok(())
}

fn validate_thumbnail(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-thumbnail
    let thumbnail = &config.thumbnail;
    for (key, value) in [
        ("thumbnail.width", u64::from(thumbnail.width)),
        ("thumbnail.height", u64::from(thumbnail.height)),
        ("thumbnail.max_bytes", to_u64(thumbnail.max_bytes)),
        ("thumbnail.max_pixels", thumbnail.max_pixels),
        (
            "thumbnail.max_decode_bytes",
            to_u64(thumbnail.max_decode_bytes),
        ),
    ] {
        positive(key, value)?;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-thumbnail
    Ok(())
}

fn validate_knowledge_search(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-knowledge
    let knowledge = &config.knowledge_search;
    if knowledge.enabled {
        for (key, value) in [
            (
                "knowledge_search.vector_store_id",
                knowledge.vector_store_id.as_deref(),
            ),
            (
                "knowledge_search.provider_id",
                knowledge.provider_id.as_deref(),
            ),
        ] {
            if value.is_none_or(str::is_empty) {
                return Err(ConfigError::invalid(
                    key,
                    "is required and must not be empty when knowledge_search.enabled is true",
                ));
            }
        }
    }
    for (key, value) in [
        (
            "knowledge_search.max_calls_per_message",
            u64::from(knowledge.max_calls_per_message),
        ),
        ("knowledge_search.top_k", to_u64(knowledge.top_k)),
        (
            "knowledge_search.max_chunk_chars",
            to_u64(knowledge.max_chunk_chars),
        ),
    ] {
        positive(key, value)?;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-knowledge
    Ok(())
}

fn validate_watchdog(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-watchdog
    let watchdog = &config.orphan_watchdog;
    in_range(
        "orphan_watchdog.timeout_secs",
        watchdog.timeout_secs,
        90,
        3600,
    )?;
    in_range(
        "orphan_watchdog.scan_interval_secs",
        watchdog.scan_interval_secs,
        1,
        3600,
    )?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-watchdog
    Ok(())
}

fn validate_reaper(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-reaper
    let reaper = &config.upload_reaper;
    in_range(
        "upload_reaper.scan_interval_secs",
        reaper.scan_interval_secs,
        1,
        3600,
    )?;
    in_range(
        "upload_reaper.stale_after_secs",
        reaper.stale_after_secs,
        60,
        86400,
    )?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-reaper
    Ok(())
}

fn validate_summary_worker(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-summary-worker
    let worker = &config.thread_summary_worker;
    in_range(
        "thread_summary_worker.claim_timeout_secs",
        worker.claim_timeout_secs,
        30,
        3600,
    )?;
    positive(
        "thread_summary_worker.max_attempts",
        u64::from(worker.max_attempts),
    )?;
    in_range(
        "thread_summary_worker.compression_threshold_pct",
        u64::from(worker.compression_threshold_pct),
        1,
        99,
    )?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-summary-worker
    Ok(())
}

fn validate_cleanup(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-cleanup
    positive(
        "cleanup_worker.max_attempts",
        u64::from(config.cleanup_worker.max_attempts),
    )?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-cleanup
    Ok(())
}

fn warn_deprecated_workers(config: &MiniChatConfig, warnings: &mut Warnings) {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-warn-deprecated-workers
    let cleanup = &config.cleanup_worker;
    if !cleanup.enabled {
        warnings.push(
            "cleanup_worker.enabled",
            "deprecated key has no effect: cleanup always runs",
        );
    }
    warnings.deprecated_if_changed(
        "cleanup_worker.poll_interval_secs",
        &cleanup.poll_interval_secs,
        &60,
    );
    warnings.deprecated_if_changed(
        "cleanup_worker.reconcile_interval_secs",
        &cleanup.reconcile_interval_secs,
        &300,
    );
    warnings.deprecated_if_changed(
        "cleanup_worker.stale_in_progress_timeout_secs",
        &cleanup.stale_in_progress_timeout_secs,
        &900,
    );
    warnings.deprecated_if_changed("cleanup_worker.batch_size", &cleanup.batch_size, &32);
    warnings.deprecated_if_changed(
        "thread_summary_worker.reconcile_interval_secs",
        &config.thread_summary_worker.reconcile_interval_secs,
        &60,
    );
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-warn-deprecated-workers
}

fn validate_outbox(config: &MiniChatConfig) -> Result<(), ConfigError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-outbox
    let outbox = &config.outbox;
    for (key, value) in [
        ("outbox.queue_name", &outbox.queue_name),
        ("outbox.cleanup_queue_name", &outbox.cleanup_queue_name),
        (
            "outbox.chat_cleanup_queue_name",
            &outbox.chat_cleanup_queue_name,
        ),
        (
            "outbox.thread_summary_queue_name",
            &outbox.thread_summary_queue_name,
        ),
        ("outbox.audit_queue_name", &outbox.audit_queue_name),
    ] {
        non_empty(key, value)?;
    }
    let partitions = u64::from(outbox.num_partitions);
    in_range("outbox.num_partitions", partitions, 1, 64)?;
    if !partitions.is_power_of_two() {
        return Err(ConfigError::invalid(
            "outbox.num_partitions",
            "must be a power of two",
        ));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-outbox
    Ok(())
}

/// Runs the range and presence rules of every section (steps 4 and 6 to 19 of
/// the algorithm) on a configuration whose credentials are already expanded.
pub(super) fn validate_sections(config: &MiniChatConfig) -> Result<Warnings, ConfigError> {
    let mut warnings = Warnings::default();
    validate_top_level(config)?;
    validate_streaming(config)?;
    validate_floor(config)?;
    warn_deprecated_budgets(config, &mut warnings);
    validate_quota(config)?;
    validate_context(config)?;
    validate_rag(config)?;
    validate_thumbnail(config)?;
    validate_knowledge_search(config)?;
    validate_watchdog(config)?;
    validate_reaper(config)?;
    validate_summary_worker(config)?;
    validate_cleanup(config)?;
    warn_deprecated_workers(config, &mut warnings);
    validate_outbox(config)?;
    Ok(warnings)
}
