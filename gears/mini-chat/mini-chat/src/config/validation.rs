//! Validation of the gear configuration (DESIGN Appendix B) and deprecation warnings.

use std::collections::HashMap;

use secrecy::ExposeSecret;
use thiserror::Error;
use uuid::Uuid;

use super::{
    CleanupWorkerConfig, EstimationBudgetsConfig, KnowledgeSearchConfig, MiniChatConfig,
    OrphanWatchdogConfig, OutboxConfig, ProviderEntry, QuotaConfig, RagConfig, StorageKind,
    StreamingConfig, ThreadSummaryWorkerConfig, ThumbnailConfig, UploadReaperConfig,
};

/// A configuration validation failure; the message names the offending key.
#[derive(Debug, Clone, Error)]
#[error("invalid mini-chat configuration: {0}")]
pub struct ConfigValidationError(String);

impl ConfigValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

type Check = Result<(), ConfigValidationError>;

fn require(condition: bool, message: impl FnOnce() -> String) -> Check {
    if condition {
        Ok(())
    } else {
        Err(ConfigValidationError::new(message()))
    }
}

fn non_empty(value: &str, key: &str) -> Check {
    require(!value.trim().is_empty(), || {
        format!("{key} must be non-empty")
    })
}

fn positive(value: u64, key: &str) -> Check {
    require(value > 0, || format!("{key} must be > 0"))
}

fn in_range(value: u64, range: std::ops::RangeInclusive<u64>, key: &str) -> Check {
    require(range.contains(&value), || {
        format!(
            "{key} must be in {}..={}, got {value}",
            range.start(),
            range.end()
        )
    })
}

/// Characters allowed in a provider host after `${VAR}` expansion: the host is the OAGW alias in
/// `/{alias}/...`, so `/`, `?`, `#` and `@` would change the proxied path.
fn is_host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']')
}

fn check_host(host: &str, key: &str) -> Check {
    non_empty(host, key)?;
    require(host.chars().all(is_host_char), || {
        format!("{key} may only contain letters, digits and `.-_:[]`, got `{host}`")
    })
}

fn check_api_version(version: &str, key: &str) -> Check {
    non_empty(version, key)?;
    require(
        version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')),
        || format!("{key} may only contain letters, digits, `.` and `-`, got `{version}`"),
    )
}

impl MiniChatConfig {
    /// Validates every section.
    ///
    /// # Errors
    /// Returns the first violated rule, naming the offending key.
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        self.validate_identity()?;
        validate_streaming(&self.streaming)?;
        validate_estimation_budgets(&self.estimation_budgets, &self.streaming)?;
        validate_quota(&self.quota)?;
        validate_outbox(&self.outbox)?;
        require(self.context.recent_messages_limit <= 100, || {
            "context.recent_messages_limit must be in 0..=100".to_owned()
        })?;
        validate_rag(&self.rag)?;
        validate_thumbnail(&self.thumbnail)?;
        validate_knowledge_search(&self.knowledge_search)?;
        validate_orphan_watchdog(&self.orphan_watchdog)?;
        validate_upload_reaper(&self.upload_reaper)?;
        validate_thread_summary_worker(&self.thread_summary_worker)?;
        validate_cleanup_worker(&self.cleanup_worker)?;
        validate_providers(&self.providers)
    }

    fn validate_identity(&self) -> Check {
        non_empty(&self.vendor, "vendor")?;
        validate_url_prefix(&self.url_prefix)?;
        non_empty(
            &self.client_credentials.client_id,
            "client_credentials.client_id",
        )?;
        non_empty(
            self.client_credentials.client_secret.expose_secret(),
            "client_credentials.client_secret",
        )
    }

    /// Warnings for deprecated fields that are set to a non-default value (they have no effect).
    #[must_use]
    pub fn deprecation_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        estimation_budget_warnings(&self.estimation_budgets, &mut warnings);
        cleanup_worker_warnings(&self.cleanup_worker, &mut warnings);
        if self.thread_summary_worker.reconcile_interval_secs
            != ThreadSummaryWorkerConfig::default().reconcile_interval_secs
        {
            warnings.push(deprecated("thread_summary_worker.reconcile_interval_secs"));
        }
        warnings
    }
}

fn deprecated(key: &str) -> String {
    format!("configuration key `{key}` is deprecated and has no effect")
}

fn validate_url_prefix(prefix: &str) -> Check {
    require(prefix.starts_with('/') && !prefix.ends_with('/'), || {
        "url_prefix must start with `/` and must not end with `/`".to_owned()
    })?;
    require(
        prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.')),
        || "url_prefix may only contain letters, digits and `/-_.`".to_owned(),
    )
}

fn validate_streaming(cfg: &StreamingConfig) -> Check {
    in_range(
        u64::from(cfg.sse_ping_interval_seconds),
        5..=60,
        "streaming.sse_ping_interval_seconds",
    )?;
    in_range(
        u64::from(cfg.sse_channel_capacity),
        16..=64,
        "streaming.sse_channel_capacity",
    )
}

fn validate_estimation_budgets(
    cfg: &EstimationBudgetsConfig,
    streaming: &StreamingConfig,
) -> Check {
    positive(
        u64::from(cfg.minimal_generation_floor),
        "estimation_budgets.minimal_generation_floor",
    )?;
    require(
        cfg.minimal_generation_floor <= streaming.max_output_tokens,
        || {
            format!(
                "estimation_budgets.minimal_generation_floor ({}) must be <= \
                 streaming.max_output_tokens ({})",
                cfg.minimal_generation_floor, streaming.max_output_tokens
            )
        },
    )
}

fn validate_quota(cfg: &QuotaConfig) -> Check {
    require(
        (1.0..=1.5).contains(&cfg.overshoot_tolerance_factor),
        || {
            format!(
                "quota.overshoot_tolerance_factor must be in 1.0..=1.5, got {}",
                cfg.overshoot_tolerance_factor
            )
        },
    )?;
    in_range(
        u64::from(cfg.warning_threshold_pct),
        1..=99,
        "quota.warning_threshold_pct",
    )?;
    positive(
        u64::from(cfg.web_search_max_calls_per_message),
        "quota.web_search_max_calls_per_message",
    )?;
    positive(
        u64::from(cfg.web_search_daily_quota),
        "quota.web_search_daily_quota",
    )?;
    positive(
        u64::from(cfg.code_interpreter_max_calls_per_message),
        "quota.code_interpreter_max_calls_per_message",
    )?;
    positive(
        u64::from(cfg.code_interpreter_daily_quota),
        "quota.code_interpreter_daily_quota",
    )
}

fn validate_outbox(cfg: &OutboxConfig) -> Check {
    non_empty(&cfg.queue_name, "outbox.queue_name")?;
    non_empty(&cfg.cleanup_queue_name, "outbox.cleanup_queue_name")?;
    non_empty(
        &cfg.chat_cleanup_queue_name,
        "outbox.chat_cleanup_queue_name",
    )?;
    non_empty(
        &cfg.thread_summary_queue_name,
        "outbox.thread_summary_queue_name",
    )?;
    non_empty(&cfg.audit_queue_name, "outbox.audit_queue_name")?;
    require(
        (1..=64).contains(&cfg.num_partitions) && cfg.num_partitions.is_power_of_two(),
        || {
            format!(
                "outbox.num_partitions must be a power of two in 1..=64, got {}",
                cfg.num_partitions
            )
        },
    )
}

fn validate_rag(cfg: &RagConfig) -> Check {
    positive(
        u64::from(cfg.max_documents_per_chat),
        "rag.max_documents_per_chat",
    )?;
    positive(
        u64::from(cfg.max_total_upload_mb_per_chat),
        "rag.max_total_upload_mb_per_chat",
    )?;
    in_range(
        u64::from(cfg.max_concurrent_uploads),
        1..=256,
        "rag.max_concurrent_uploads",
    )?;
    positive(
        u64::from(cfg.uploaded_file_max_size_kb),
        "rag.uploaded_file_max_size_kb",
    )?;
    positive(
        u64::from(cfg.uploaded_image_max_size_kb),
        "rag.uploaded_image_max_size_kb",
    )?;
    positive(
        u64::from(cfg.max_images_per_message),
        "rag.max_images_per_message",
    )
}

fn validate_thumbnail(cfg: &ThumbnailConfig) -> Check {
    positive(u64::from(cfg.width), "thumbnail.width")?;
    positive(u64::from(cfg.height), "thumbnail.height")?;
    require(cfg.max_bytes > 0, || {
        "thumbnail.max_bytes must be > 0".to_owned()
    })?;
    positive(cfg.max_pixels, "thumbnail.max_pixels")?;
    require(cfg.max_decode_bytes > 0, || {
        "thumbnail.max_decode_bytes must be > 0".to_owned()
    })
}

fn validate_knowledge_search(cfg: &KnowledgeSearchConfig) -> Check {
    if cfg.enabled {
        non_empty(
            cfg.vector_store_id.as_deref().unwrap_or_default(),
            "knowledge_search.vector_store_id (required when enabled)",
        )?;
        non_empty(
            cfg.provider_id.as_deref().unwrap_or_default(),
            "knowledge_search.provider_id (required when enabled)",
        )?;
    }
    positive(
        u64::from(cfg.max_calls_per_message),
        "knowledge_search.max_calls_per_message",
    )?;
    require(cfg.top_k > 0, || {
        "knowledge_search.top_k must be > 0".to_owned()
    })?;
    require(cfg.max_chunk_chars > 0, || {
        "knowledge_search.max_chunk_chars must be > 0".to_owned()
    })
}

fn validate_orphan_watchdog(cfg: &OrphanWatchdogConfig) -> Check {
    in_range(cfg.timeout_secs, 90..=3600, "orphan_watchdog.timeout_secs")?;
    in_range(
        cfg.scan_interval_secs,
        1..=3600,
        "orphan_watchdog.scan_interval_secs",
    )
}

fn validate_upload_reaper(cfg: &UploadReaperConfig) -> Check {
    in_range(
        cfg.scan_interval_secs,
        1..=3600,
        "upload_reaper.scan_interval_secs",
    )?;
    in_range(
        cfg.stale_after_secs,
        60..=86_400,
        "upload_reaper.stale_after_secs",
    )
}

fn validate_thread_summary_worker(cfg: &ThreadSummaryWorkerConfig) -> Check {
    in_range(
        cfg.claim_timeout_secs,
        30..=3600,
        "thread_summary_worker.claim_timeout_secs",
    )?;
    positive(
        u64::from(cfg.max_attempts),
        "thread_summary_worker.max_attempts",
    )?;
    in_range(
        u64::from(cfg.compression_threshold_pct),
        1..=99,
        "thread_summary_worker.compression_threshold_pct",
    )
}

fn validate_cleanup_worker(cfg: &CleanupWorkerConfig) -> Check {
    positive(u64::from(cfg.max_attempts), "cleanup_worker.max_attempts")
}

fn validate_providers(providers: &HashMap<String, ProviderEntry>) -> Check {
    for (id, entry) in providers {
        non_empty(id, "providers.<id>")?;
        validate_provider(id, entry, providers)?;
    }
    Ok(())
}

fn validate_provider(
    id: &str,
    entry: &ProviderEntry,
    all: &HashMap<String, ProviderEntry>,
) -> Check {
    check_host(&entry.host, &format!("providers.{id}.host"))?;
    require(entry.port != Some(0), || {
        format!("providers.{id}.port must not be 0")
    })?;
    if let Some(alias) = &entry.upstream_alias {
        non_empty(alias, &format!("providers.{id}.upstream_alias"))?;
    }
    non_empty(&entry.api_path, &format!("providers.{id}.api_path"))?;
    if entry.storage_kind == StorageKind::Azure {
        let key = format!("providers.{id}.api_version (required for storage_kind = azure)");
        check_api_version(entry.api_version.as_deref().unwrap_or_default(), &key)?;
    }
    if let Some(rag) = &entry.rag_provider {
        require(all.contains_key(rag), || {
            format!("providers.{id}.rag_provider names `{rag}`, which is not a provider entry")
        })?;
    }
    for (tenant, over) in &entry.tenant_overrides {
        validate_tenant_override(id, tenant, over)?;
    }
    Ok(())
}

fn validate_tenant_override(provider: &str, tenant: &str, over: &super::TenantOverride) -> Check {
    let key = format!("providers.{provider}.tenant_overrides.{tenant}");
    require(Uuid::parse_str(tenant).is_ok(), || {
        format!("{key}: the key must be a tenant id (UUID)")
    })?;
    require(over.host.is_some() || over.upstream_alias.is_some(), || {
        format!("{key} must set host or upstream_alias")
    })?;
    if let Some(host) = &over.host {
        check_host(host, &format!("{key}.host"))?;
    }
    if let Some(alias) = &over.upstream_alias {
        non_empty(alias, &format!("{key}.upstream_alias"))?;
    }
    Ok(())
}

fn estimation_budget_warnings(cfg: &EstimationBudgetsConfig, warnings: &mut Vec<String>) {
    let default = EstimationBudgetsConfig::default();
    let fields = [
        (
            "bytes_per_token_conservative",
            cfg.bytes_per_token_conservative != default.bytes_per_token_conservative,
        ),
        (
            "fixed_overhead_tokens",
            cfg.fixed_overhead_tokens != default.fixed_overhead_tokens,
        ),
        (
            "safety_margin_pct",
            cfg.safety_margin_pct != default.safety_margin_pct,
        ),
        (
            "image_token_budget",
            cfg.image_token_budget != default.image_token_budget,
        ),
        (
            "tool_surcharge_tokens",
            cfg.tool_surcharge_tokens != default.tool_surcharge_tokens,
        ),
        (
            "web_search_surcharge_tokens",
            cfg.web_search_surcharge_tokens != default.web_search_surcharge_tokens,
        ),
        (
            "code_interpreter_surcharge_tokens",
            cfg.code_interpreter_surcharge_tokens != default.code_interpreter_surcharge_tokens,
        ),
    ];
    warnings.extend(
        fields
            .into_iter()
            .filter(|(_, changed)| *changed)
            .map(|(name, _)| deprecated(&format!("estimation_budgets.{name}"))),
    );
}

fn cleanup_worker_warnings(cfg: &CleanupWorkerConfig, warnings: &mut Vec<String>) {
    let default = CleanupWorkerConfig::default();
    let fields = [
        ("enabled", cfg.enabled != default.enabled),
        (
            "poll_interval_secs",
            cfg.poll_interval_secs != default.poll_interval_secs,
        ),
        (
            "reconcile_interval_secs",
            cfg.reconcile_interval_secs != default.reconcile_interval_secs,
        ),
        (
            "stale_in_progress_timeout_secs",
            cfg.stale_in_progress_timeout_secs != default.stale_in_progress_timeout_secs,
        ),
        ("batch_size", cfg.batch_size != default.batch_size),
    ];
    warnings.extend(
        fields
            .into_iter()
            .filter(|(_, changed)| *changed)
            .map(|(name, _)| deprecated(&format!("cleanup_worker.{name}"))),
    );
}
