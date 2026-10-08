//! Gear configuration (`gears.mini-chat.config`), see DESIGN Appendix B.
//!
//! Sections of the user-facing gear config reject unknown keys; the worker sections
//! (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`, `cleanup_worker`) accept and
//! ignore them. Validation runs once at gear init via [`MiniChatConfig::validate`].

pub mod providers;

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use toolkit::var_expand::{ExpandVars, ExpandVarsError};

pub use providers::{
    APIKEY_AUTH_PLUGIN, ProviderEntry, ProviderKind, StorageKind, TenantOverride, default_providers,
};

/// Default web-search guard instruction (appended to the system prompt).
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";
/// Default file-search guard instruction (appended to the system prompt).
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search to find relevant excerpts in the documents uploaded to this chat when the user asks about them. Cite the documents you use.";
/// Default knowledge-search guard instruction (appended to the system prompt).
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str =
    "Use search_knowledge to look up organization knowledge when the question needs it.";
/// Built-in system prompt of the thread-summary request (DESIGN B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// S2S credentials exchanged through `authn_resolver` for OAGW provisioning.
#[derive(Clone, Default, Deserialize, toolkit::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCredentials {
    #[expand_vars]
    pub client_id: String,
    #[expand_vars]
    pub client_secret: String,
}

impl fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

/// `metrics` section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix; empty means `mini_chat`.
    pub prefix: String,
}

/// `streaming` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    pub sse_ping_interval_seconds: u16,
    pub sse_channel_capacity: u16,
    pub max_output_tokens: u32,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            sse_ping_interval_seconds: 15,
            sse_channel_capacity: 32,
            max_output_tokens: 32768,
        }
    }
}

/// `estimation_budgets` section. Only `minimal_generation_floor` is used; the other fields are
/// deprecated (parsed, not validated, warned about when non-default).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EstimationBudgetsConfig {
    pub bytes_per_token_conservative: u32,
    pub fixed_overhead_tokens: u32,
    pub safety_margin_pct: u32,
    pub image_token_budget: u32,
    pub tool_surcharge_tokens: u32,
    pub web_search_surcharge_tokens: u32,
    pub code_interpreter_surcharge_tokens: u32,
    pub minimal_generation_floor: u32,
}

impl Default for EstimationBudgetsConfig {
    fn default() -> Self {
        Self {
            bytes_per_token_conservative: 4,
            fixed_overhead_tokens: 100,
            safety_margin_pct: 10,
            image_token_budget: 1000,
            tool_surcharge_tokens: 500,
            web_search_surcharge_tokens: 500,
            code_interpreter_surcharge_tokens: 1000,
            minimal_generation_floor: 50,
        }
    }
}

/// `quota` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    pub overshoot_tolerance_factor: f64,
    pub warning_threshold_pct: u8,
    pub web_search_max_calls_per_message: u32,
    pub web_search_daily_quota: u32,
    pub code_interpreter_max_calls_per_message: u32,
    pub code_interpreter_daily_quota: u32,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            overshoot_tolerance_factor: 1.10,
            warning_threshold_pct: 80,
            web_search_max_calls_per_message: 2,
            web_search_daily_quota: 75,
            code_interpreter_max_calls_per_message: 10,
            code_interpreter_daily_quota: 50,
        }
    }
}

/// `outbox` section: queue names and partition count shared by all five queues.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutboxConfig {
    pub queue_name: String,
    pub cleanup_queue_name: String,
    pub chat_cleanup_queue_name: String,
    pub thread_summary_queue_name: String,
    pub audit_queue_name: String,
    pub num_partitions: u32,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            queue_name: "mini-chat.usage_snapshot".to_owned(),
            cleanup_queue_name: "mini-chat.attachment_cleanup".to_owned(),
            chat_cleanup_queue_name: "mini-chat.chat_cleanup".to_owned(),
            thread_summary_queue_name: "mini-chat.thread_summary".to_owned(),
            audit_queue_name: "mini-chat.audit".to_owned(),
            num_partitions: 4,
        }
    }
}

/// `context` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub recent_messages_limit: u32,
    pub web_search_guard: String,
    pub file_search_guard: String,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            recent_messages_limit: 10,
            web_search_guard: DEFAULT_WEB_SEARCH_GUARD.to_owned(),
            file_search_guard: DEFAULT_FILE_SEARCH_GUARD.to_owned(),
        }
    }
}

/// `rag` section (documents, images, uploads).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RagConfig {
    pub max_documents_per_chat: u32,
    pub max_total_upload_mb_per_chat: u32,
    pub allow_csv_upload: bool,
    pub max_concurrent_uploads: u16,
    pub uploaded_file_max_size_kb: u32,
    pub uploaded_image_max_size_kb: u32,
    pub max_images_per_message: u32,
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            max_documents_per_chat: 50,
            max_total_upload_mb_per_chat: 100,
            allow_csv_upload: true,
            max_concurrent_uploads: 10,
            uploaded_file_max_size_kb: 25600,
            uploaded_image_max_size_kb: 5120,
            max_images_per_message: 4,
        }
    }
}

/// `thumbnail` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThumbnailConfig {
    pub width: u32,
    pub height: u32,
    pub max_bytes: usize,
    pub max_pixels: u64,
    pub max_decode_bytes: usize,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            width: 128,
            height: 128,
            max_bytes: 131_072,
            max_pixels: 100_000_000,
            max_decode_bytes: 33_554_432,
        }
    }
}

/// `orphan_watchdog` section (unknown keys accepted).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrphanWatchdogConfig {
    pub enabled: bool,
    pub timeout_secs: u64,
    pub scan_interval_secs: u64,
}

impl Default for OrphanWatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeout_secs: 300,
            scan_interval_secs: 60,
        }
    }
}

/// Smallest valid `upload_reaper.stale_after_secs`.
pub const MIN_UPLOAD_STALE_AFTER_SECS: u64 = 60;

/// `upload_reaper` section (unknown keys accepted).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UploadReaperConfig {
    pub enabled: bool,
    pub scan_interval_secs: u64,
    pub stale_after_secs: u64,
}

impl Default for UploadReaperConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_interval_secs: 60,
            stale_after_secs: 300,
        }
    }
}

/// `thread_summary_worker` section (unknown keys accepted).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThreadSummaryWorkerConfig {
    pub enabled: bool,
    pub claim_timeout_secs: u64,
    pub max_attempts: u32,
    pub compression_threshold_pct: u32,
    /// Empty means `gpt-4.1-mini`.
    pub summary_model_id: String,
    pub summary_system_prompt: String,
    pub message_content_limit: usize,
    /// Deprecated, no effect.
    pub reconcile_interval_secs: u64,
}

impl Default for ThreadSummaryWorkerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            claim_timeout_secs: 300,
            max_attempts: 3,
            compression_threshold_pct: 80,
            summary_model_id: String::new(),
            summary_system_prompt: DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned(),
            message_content_limit: 4000,
            reconcile_interval_secs: 60,
        }
    }
}

/// `cleanup_worker` section (unknown keys accepted). Only `max_attempts` has an effect.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CleanupWorkerConfig {
    pub max_attempts: u32,
    /// Deprecated, no effect.
    pub enabled: bool,
    /// Deprecated, no effect.
    pub poll_interval_secs: u64,
    /// Deprecated, no effect.
    pub reconcile_interval_secs: u64,
    /// Deprecated, no effect.
    pub stale_in_progress_timeout_secs: u64,
    /// Deprecated, no effect.
    pub batch_size: u32,
}

impl Default for CleanupWorkerConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            enabled: true,
            poll_interval_secs: 60,
            reconcile_interval_secs: 300,
            stale_in_progress_timeout_secs: 900,
            batch_size: 32,
        }
    }
}

/// `knowledge_search` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KnowledgeSearchConfig {
    pub enabled: bool,
    pub vector_store_id: Option<String>,
    pub provider_id: Option<String>,
    pub max_calls_per_message: u32,
    pub top_k: usize,
    pub max_chunk_chars: usize,
    pub guard: String,
}

impl Default for KnowledgeSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vector_store_id: None,
            provider_id: None,
            max_calls_per_message: 3,
            top_k: 5,
            max_chunk_chars: 2000,
            guard: DEFAULT_KNOWLEDGE_SEARCH_GUARD.to_owned(),
        }
    }
}

/// Root of `gears.mini-chat.config`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    pub url_prefix: String,
    pub vendor: String,
    pub client_credentials: ClientCredentials,
    pub metrics: MetricsConfig,
    pub providers: BTreeMap<String, ProviderEntry>,
    pub streaming: StreamingConfig,
    pub estimation_budgets: EstimationBudgetsConfig,
    pub quota: QuotaConfig,
    pub outbox: OutboxConfig,
    pub context: ContextConfig,
    pub rag: RagConfig,
    pub thumbnail: ThumbnailConfig,
    pub orphan_watchdog: OrphanWatchdogConfig,
    pub upload_reaper: UploadReaperConfig,
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    pub cleanup_worker: CleanupWorkerConfig,
    pub knowledge_search: KnowledgeSearchConfig,
}

impl Default for MiniChatConfig {
    fn default() -> Self {
        Self {
            url_prefix: "/mini-chat".to_owned(),
            vendor: "constructorfabric".to_owned(),
            client_credentials: ClientCredentials::default(),
            metrics: MetricsConfig::default(),
            providers: default_providers(),
            streaming: StreamingConfig::default(),
            estimation_budgets: EstimationBudgetsConfig::default(),
            quota: QuotaConfig::default(),
            outbox: OutboxConfig::default(),
            context: ContextConfig::default(),
            rag: RagConfig::default(),
            thumbnail: ThumbnailConfig::default(),
            orphan_watchdog: OrphanWatchdogConfig::default(),
            upload_reaper: UploadReaperConfig::default(),
            thread_summary_worker: ThreadSummaryWorkerConfig::default(),
            cleanup_worker: CleanupWorkerConfig::default(),
            knowledge_search: KnowledgeSearchConfig::default(),
        }
    }
}

// `${VAR}` expansion: provider `host` / `auth_config` (entry and tenant overrides) and
// `client_credentials`. Hand-written because `providers` is a `BTreeMap`, which the toolkit
// derive cannot walk.
impl ExpandVars for MiniChatConfig {
    fn expand_vars(&mut self) -> Result<(), ExpandVarsError> {
        self.client_credentials.expand_vars()?;
        for entry in self.providers.values_mut() {
            entry.expand_vars()?;
        }
        Ok(())
    }
}

fn require(ok: bool, msg: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(msg()) }
}

fn require_range<T: PartialOrd + fmt::Display + Copy>(
    name: &str,
    value: T,
    min: T,
    max: T,
) -> Result<(), String> {
    require((min..=max).contains(&value), || {
        format!("{name} must be in {min}..={max}, got {value}")
    })
}

fn require_positive<T: PartialOrd + Default + fmt::Display + Copy>(
    name: &str,
    value: T,
) -> Result<(), String> {
    require(value > T::default(), || {
        format!("{name} must be > 0, got {value}")
    })
}

/// `url_prefix` is mounted verbatim in the router: empty (routes at `/v1`) or `/seg[/seg...]`
/// with non-empty segments of unreserved URL characters. Anything else (`{`, `}`, `*`, `:`, a
/// missing leading or a trailing `/`) would be read as route syntax or make axum panic.
fn validate_url_prefix(prefix: &str) -> Result<(), String> {
    if prefix.is_empty() {
        return Ok(());
    }
    let unreserved = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~');
    let valid = prefix.strip_prefix('/').is_some_and(|rest| {
        rest.split('/')
            .all(|seg| !seg.is_empty() && seg.chars().all(unreserved))
    });
    require(valid, || {
        format!(
            "url_prefix '{prefix}' must be empty or '/'-separated non-empty segments of letters, \
             digits, '-', '.', '_', '~' with a leading and no trailing '/' (e.g. /mini-chat)"
        )
    })
}

fn non_blank(value: Option<&String>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

impl MiniChatConfig {
    /// Post-load normalization (runs before [`Self::validate`]): fills `upstream_alias` on every
    /// provider entry and tenant override when unset (DESIGN B.1, [`providers::derive_alias`]).
    pub fn normalize(&mut self) {
        for entry in self.providers.values_mut() {
            entry.fill_aliases();
        }
    }

    /// Validates every section (DESIGN Appendix B).
    ///
    /// # Errors
    /// Returns a description of the first violated rule.
    pub fn validate(&self) -> Result<(), String> {
        validate_url_prefix(&self.url_prefix)?;
        require(!self.vendor.trim().is_empty(), || {
            "vendor must be non-empty".to_owned()
        })?;
        require(
            !self.client_credentials.client_id.trim().is_empty()
                && !self.client_credentials.client_secret.trim().is_empty(),
            || {
                "client_credentials.client_id and client_credentials.client_secret must be non-empty".to_owned()
            },
        )?;
        for (id, entry) in &self.providers {
            entry.validate(id, &self.providers)?;
        }
        providers::validate_alias_collisions(&self.providers)?;
        self.validate_streaming_and_quota()?;
        self.validate_limits()?;
        self.validate_workers()?;
        self.validate_knowledge_search()
    }

    fn validate_streaming_and_quota(&self) -> Result<(), String> {
        let s = &self.streaming;
        require_range(
            "streaming.sse_ping_interval_seconds",
            s.sse_ping_interval_seconds,
            5,
            60,
        )?;
        require_range(
            "streaming.sse_channel_capacity",
            s.sse_channel_capacity,
            16,
            64,
        )?;
        let floor = self.estimation_budgets.minimal_generation_floor;
        require_positive("estimation_budgets.minimal_generation_floor", floor)?;
        require(floor <= s.max_output_tokens, || {
            format!(
                "estimation_budgets.minimal_generation_floor ({floor}) must be <= streaming.max_output_tokens ({})",
                s.max_output_tokens
            )
        })?;
        let q = &self.quota;
        require_range(
            "quota.overshoot_tolerance_factor",
            q.overshoot_tolerance_factor,
            1.0,
            1.5,
        )?;
        require_range(
            "quota.warning_threshold_pct",
            q.warning_threshold_pct,
            1,
            99,
        )?;
        require_positive(
            "quota.web_search_max_calls_per_message",
            q.web_search_max_calls_per_message,
        )?;
        require_positive("quota.web_search_daily_quota", q.web_search_daily_quota)?;
        require_positive(
            "quota.code_interpreter_max_calls_per_message",
            q.code_interpreter_max_calls_per_message,
        )?;
        require_positive(
            "quota.code_interpreter_daily_quota",
            q.code_interpreter_daily_quota,
        )
    }

    fn validate_limits(&self) -> Result<(), String> {
        let o = &self.outbox;
        for (name, value) in [
            ("outbox.queue_name", &o.queue_name),
            ("outbox.cleanup_queue_name", &o.cleanup_queue_name),
            ("outbox.chat_cleanup_queue_name", &o.chat_cleanup_queue_name),
            (
                "outbox.thread_summary_queue_name",
                &o.thread_summary_queue_name,
            ),
            ("outbox.audit_queue_name", &o.audit_queue_name),
        ] {
            require(!value.trim().is_empty(), || {
                format!("{name} must be non-empty")
            })?;
        }
        require(
            (1..=64).contains(&o.num_partitions) && o.num_partitions.is_power_of_two(),
            || {
                format!(
                    "outbox.num_partitions must be a power of 2 in 1..=64, got {}",
                    o.num_partitions
                )
            },
        )?;
        require_range(
            "context.recent_messages_limit",
            self.context.recent_messages_limit,
            0,
            100,
        )?;
        let r = &self.rag;
        require_positive("rag.max_documents_per_chat", r.max_documents_per_chat)?;
        require_positive(
            "rag.max_total_upload_mb_per_chat",
            r.max_total_upload_mb_per_chat,
        )?;
        require_range(
            "rag.max_concurrent_uploads",
            r.max_concurrent_uploads,
            1,
            256,
        )?;
        require_positive("rag.uploaded_file_max_size_kb", r.uploaded_file_max_size_kb)?;
        require_positive(
            "rag.uploaded_image_max_size_kb",
            r.uploaded_image_max_size_kb,
        )?;
        require_positive("rag.max_images_per_message", r.max_images_per_message)?;
        let t = &self.thumbnail;
        require_positive("thumbnail.width", t.width)?;
        require_positive("thumbnail.height", t.height)?;
        require_positive("thumbnail.max_bytes", t.max_bytes)?;
        require_positive("thumbnail.max_pixels", t.max_pixels)?;
        require_positive("thumbnail.max_decode_bytes", t.max_decode_bytes)
    }

    fn validate_workers(&self) -> Result<(), String> {
        let w = &self.orphan_watchdog;
        require_range("orphan_watchdog.timeout_secs", w.timeout_secs, 90, 3600)?;
        require_range(
            "orphan_watchdog.scan_interval_secs",
            w.scan_interval_secs,
            1,
            3600,
        )?;
        let u = &self.upload_reaper;
        require_range(
            "upload_reaper.scan_interval_secs",
            u.scan_interval_secs,
            1,
            3600,
        )?;
        require_range(
            "upload_reaper.stale_after_secs",
            u.stale_after_secs,
            MIN_UPLOAD_STALE_AFTER_SECS,
            86400,
        )?;
        let s = &self.thread_summary_worker;
        require_range(
            "thread_summary_worker.claim_timeout_secs",
            s.claim_timeout_secs,
            30,
            3600,
        )?;
        require_positive("thread_summary_worker.max_attempts", s.max_attempts)?;
        require_range(
            "thread_summary_worker.compression_threshold_pct",
            s.compression_threshold_pct,
            1,
            99,
        )?;
        require_positive(
            "cleanup_worker.max_attempts",
            self.cleanup_worker.max_attempts,
        )
    }

    fn validate_knowledge_search(&self) -> Result<(), String> {
        let k = &self.knowledge_search;
        if k.enabled {
            require(non_blank(k.vector_store_id.as_ref()), || {
                "knowledge_search.vector_store_id is required when enabled".to_owned()
            })?;
            require(non_blank(k.provider_id.as_ref()), || {
                "knowledge_search.provider_id is required when enabled".to_owned()
            })?;
        }
        require_positive(
            "knowledge_search.max_calls_per_message",
            k.max_calls_per_message,
        )?;
        require_positive("knowledge_search.top_k", k.top_k)?;
        require_positive("knowledge_search.max_chunk_chars", k.max_chunk_chars)
    }

    /// One message per deprecated field that is set to a non-default value (logged at gear init).
    #[must_use]
    pub fn deprecation_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut check = |name: &str, is_default: bool| {
            if !is_default {
                out.push(format!("{name} is deprecated and has no effect"));
            }
        };
        let (e, d) = (&self.estimation_budgets, EstimationBudgetsConfig::default());
        check(
            "estimation_budgets.bytes_per_token_conservative",
            e.bytes_per_token_conservative == d.bytes_per_token_conservative,
        );
        check(
            "estimation_budgets.fixed_overhead_tokens",
            e.fixed_overhead_tokens == d.fixed_overhead_tokens,
        );
        check(
            "estimation_budgets.safety_margin_pct",
            e.safety_margin_pct == d.safety_margin_pct,
        );
        check(
            "estimation_budgets.image_token_budget",
            e.image_token_budget == d.image_token_budget,
        );
        check(
            "estimation_budgets.tool_surcharge_tokens",
            e.tool_surcharge_tokens == d.tool_surcharge_tokens,
        );
        check(
            "estimation_budgets.web_search_surcharge_tokens",
            e.web_search_surcharge_tokens == d.web_search_surcharge_tokens,
        );
        check(
            "estimation_budgets.code_interpreter_surcharge_tokens",
            e.code_interpreter_surcharge_tokens == d.code_interpreter_surcharge_tokens,
        );
        let (c, d) = (&self.cleanup_worker, CleanupWorkerConfig::default());
        check("cleanup_worker.enabled", c.enabled == d.enabled);
        check(
            "cleanup_worker.poll_interval_secs",
            c.poll_interval_secs == d.poll_interval_secs,
        );
        check(
            "cleanup_worker.reconcile_interval_secs",
            c.reconcile_interval_secs == d.reconcile_interval_secs,
        );
        check(
            "cleanup_worker.stale_in_progress_timeout_secs",
            c.stale_in_progress_timeout_secs == d.stale_in_progress_timeout_secs,
        );
        check("cleanup_worker.batch_size", c.batch_size == d.batch_size);
        check(
            "thread_summary_worker.reconcile_interval_secs",
            self.thread_summary_worker.reconcile_interval_secs
                == ThreadSummaryWorkerConfig::default().reconcile_interval_secs,
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use toolkit::var_expand::ExpandVars;

    fn creds() -> Value {
        json!({"client_id": "mini-chat", "client_secret": "secret"})
    }

    /// Deserialize, expand `${VAR}`, normalize and validate - the same steps as gear `init`.
    fn load(value: Value) -> Result<MiniChatConfig, String> {
        let mut cfg: MiniChatConfig = serde_json::from_value(value).map_err(|e| e.to_string())?;
        cfg.expand_vars().map_err(|e| e.to_string())?;
        cfg.normalize();
        cfg.validate()?;
        Ok(cfg)
    }

    #[allow(clippy::needless_pass_by_value)] // call sites pass `json!` literals
    fn with(extra: Value) -> Value {
        let mut base = json!({ "client_credentials": creds() });
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    }

    #[allow(clippy::needless_pass_by_value)] // call sites pass `json!` literals
    fn provider(extra: Value) -> Value {
        let mut p = json!({"kind": "openai_responses", "host": "api.example.com"});
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        p
    }

    #[allow(clippy::needless_pass_by_value)] // call sites pass `json!` literals
    fn with_providers(providers: Value) -> Value {
        with(json!({ "providers": providers }))
    }

    #[test]
    fn normalize_fills_upstream_alias_on_entries_and_overrides() {
        let cfg = load(with_providers(json!({
            "p": provider(json!({
                "host": "api.example.com", "port": 8443,
                "tenant_overrides": {
                    "t1": {"host": "eu.example.com"},
                    "t2": {"upstream_alias": "Keep.Me"}
                }
            })),
            "q": provider(json!({"host": "127.0.0.1", "port": 8080, "use_http": true})),
        })))
        .unwrap();
        let p = &cfg.providers["p"];
        assert_eq!(p.upstream_alias.as_deref(), Some("api.example.com:8443"));
        assert_eq!(
            p.tenant_overrides["t1"].upstream_alias.as_deref(),
            Some("eu.example.com:8443")
        );
        assert_eq!(
            p.tenant_overrides["t2"].upstream_alias.as_deref(),
            Some("Keep.Me")
        );
        assert_eq!(
            cfg.providers["q"].upstream_alias.as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn config_mini_chat_yaml_parses() {
        use figment::Figment;
        use figment::providers::{Format, Yaml};

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../config/mini-chat.yaml"
        );
        let root: Value = Figment::new().merge(Yaml::file(path)).extract().unwrap();
        let section = root["gears"]["mini-chat"]["config"].clone();
        assert!(section.is_object(), "gears.mini-chat.config must exist");

        temp_env::with_var(
            "AZURE_OPENAI_API_HOST",
            Some("res.openai.azure.com"),
            || {
                let cfg = load(section.clone()).unwrap();
                let azure = &cfg.providers["azure_openai"];
                assert_eq!(azure.host, "res.openai.azure.com");
                assert_eq!(azure.storage_kind, Some(StorageKind::Azure));
                assert_eq!(cfg.orphan_watchdog.timeout_secs, 90);
            },
        );

        // Without the variable the `${...}` expansion fails instead of leaking the placeholder.
        temp_env::with_var_unset("AZURE_OPENAI_API_HOST", || {
            assert!(load(section.clone()).is_err());
        });
    }

    #[test]
    fn config_e2e_local_minimal_parses() {
        let cfg = load(json!({"client_credentials": creds()})).unwrap();
        assert_eq!(cfg.url_prefix, "/mini-chat");
        assert_eq!(cfg.vendor, "constructorfabric");
        assert_eq!(cfg.providers.len(), 1);
        let openai = &cfg.providers["openai"];
        assert_eq!(openai.kind, ProviderKind::OpenaiResponses);
        assert_eq!(openai.host, "api.openai.com");
        assert_eq!(openai.storage_kind, Some(StorageKind::Openai));
        assert_eq!(openai.api_path, "/v1/responses");
        assert_eq!(
            openai.auth_config.as_ref().unwrap()["secret_ref"],
            "cred://openai-key"
        );
        assert_eq!(cfg.quota.web_search_daily_quota, 75);
        assert_eq!(cfg.outbox.num_partitions, 4);
        assert_eq!(cfg.outbox.queue_name, "mini-chat.usage_snapshot");
        assert_eq!(cfg.streaming.sse_ping_interval_seconds, 15);
        assert_eq!(cfg.estimation_budgets.minimal_generation_floor, 50);
        assert!(cfg.deprecation_warnings().is_empty());
    }

    #[test]
    fn config_rejects_unknown_top_level_and_provider_keys() {
        assert!(load(with(json!({"mcp": {}}))).is_err());
        assert!(
            load(with_providers(json!({
                "p": provider(json!({"supports_file_search_filters": true}))
            })))
            .is_err()
        );
        assert!(
            load(with(
                json!({"streaming": {"web_search_context_size": "low"}})
            ))
            .is_err()
        );
        assert!(
            load(with_providers(json!({
                "p": provider(json!({"tenant_overrides": {"t": {"host": "h", "bogus": 1}}}))
            })))
            .is_err()
        );
        assert!(load(with(json!({"quota": {"nope": 1}}))).is_err());
        assert!(
            load(json!({"client_credentials": {"client_id": "a", "client_secret": "b", "x": 1}}))
                .is_err()
        );
    }

    #[test]
    fn config_worker_sections_accept_unknown_keys() {
        let cfg = load(with(json!({
            "orphan_watchdog": {"foo": 1},
            "upload_reaper": {"foo": 1},
            "thread_summary_worker": {"foo": 1},
            "cleanup_worker": {"foo": 1},
        })))
        .unwrap();
        assert_eq!(cfg.orphan_watchdog.timeout_secs, 300);
    }

    #[test]
    fn config_validation_ranges() {
        let bad: Vec<(&str, Value)> = vec![
            (
                "watchdog timeout",
                with(json!({"orphan_watchdog": {"timeout_secs": 60}})),
            ),
            (
                "watchdog scan",
                with(json!({"orphan_watchdog": {"scan_interval_secs": 0}})),
            ),
            (
                "ping interval",
                with(json!({"streaming": {"sse_ping_interval_seconds": 4}})),
            ),
            (
                "channel capacity",
                with(json!({"streaming": {"sse_channel_capacity": 65}})),
            ),
            (
                "azure without api_version",
                with_providers(json!({
                    "az": provider(json!({"storage_kind": "azure"}))
                })),
            ),
            (
                "azure blank api_version",
                with_providers(json!({
                    "az": provider(json!({"storage_kind": "azure", "api_version": "  "}))
                })),
            ),
            (
                "azure api_version chars",
                with_providers(json!({
                    "az": provider(json!({"storage_kind": "azure", "api_version": "2025&x=1"}))
                })),
            ),
            (
                "floor zero",
                with(json!({"estimation_budgets": {"minimal_generation_floor": 0}})),
            ),
            (
                "floor above max output",
                with(json!({
                    "streaming": {"max_output_tokens": 100},
                    "estimation_budgets": {"minimal_generation_floor": 101}
                })),
            ),
            ("partitions", with(json!({"outbox": {"num_partitions": 3}}))),
            (
                "partitions too many",
                with(json!({"outbox": {"num_partitions": 128}})),
            ),
            (
                "empty queue name",
                with(json!({"outbox": {"audit_queue_name": ""}})),
            ),
            (
                "overshoot",
                with(json!({"quota": {"overshoot_tolerance_factor": 1.6}})),
            ),
            (
                "warning pct",
                with(json!({"quota": {"warning_threshold_pct": 100}})),
            ),
            (
                "web search daily quota",
                with(json!({"quota": {"web_search_daily_quota": 0}})),
            ),
            (
                "rag_provider missing",
                with_providers(json!({
                    "p": provider(json!({"rag_provider": "missing"}))
                })),
            ),
            (
                "rag_provider names itself",
                with_providers(json!({
                    "p": provider(json!({"storage_kind": "openai", "rag_provider": "p"}))
                })),
            ),
            (
                "rag_provider without storage_kind",
                with_providers(json!({
                    "plain": provider(json!({})),
                    "an": provider(json!({"kind": "anthropic_messages", "rag_provider": "plain"}))
                })),
            ),
            (
                "anthropic without rag_provider",
                with_providers(json!({
                    "a": provider(json!({"kind": "anthropic_messages"}))
                })),
            ),
            (
                "host slash",
                with_providers(json!({"p": provider(json!({"host": "a/b"}))})),
            ),
            (
                "host empty",
                with_providers(json!({"p": provider(json!({"host": ""}))})),
            ),
            (
                "port zero",
                with_providers(json!({"p": provider(json!({"port": 0}))})),
            ),
            (
                "override without host or alias",
                with_providers(json!({
                    "p": provider(json!({"tenant_overrides": {"t": {}}}))
                })),
            ),
            (
                "override host chars",
                with_providers(json!({
                    "p": provider(json!({"tenant_overrides": {"t": {"host": "a@b"}}}))
                })),
            ),
            (
                "empty client_id",
                json!({"client_credentials": {"client_id": "", "client_secret": "s"}}),
            ),
            (
                "blank client_secret",
                json!({"client_credentials": {"client_id": "i", "client_secret": " "}}),
            ),
            ("missing client_credentials", json!({})),
            ("empty vendor", with(json!({"vendor": ""}))),
            (
                "recent messages",
                with(json!({"context": {"recent_messages_limit": 101}})),
            ),
            (
                "rag zero",
                with(json!({"rag": {"max_documents_per_chat": 0}})),
            ),
            (
                "concurrent uploads",
                with(json!({"rag": {"max_concurrent_uploads": 0}})),
            ),
            ("thumbnail zero", with(json!({"thumbnail": {"width": 0}}))),
            (
                "reaper stale",
                with(json!({"upload_reaper": {"stale_after_secs": 59}})),
            ),
            (
                "summary claim timeout",
                with(json!({"thread_summary_worker": {"claim_timeout_secs": 29}})),
            ),
            (
                "summary threshold",
                with(json!({"thread_summary_worker": {"compression_threshold_pct": 100}})),
            ),
            (
                "summary attempts",
                with(json!({"thread_summary_worker": {"max_attempts": 0}})),
            ),
            (
                "cleanup attempts",
                with(json!({"cleanup_worker": {"max_attempts": 0}})),
            ),
            (
                "knowledge enabled without ids",
                with(json!({"knowledge_search": {"enabled": true}})),
            ),
            (
                "knowledge top_k",
                with(json!({"knowledge_search": {"top_k": 0}})),
            ),
        ];
        for (name, value) in bad {
            assert!(
                load(value).is_err(),
                "expected validation failure for: {name}"
            );
        }

        // Boundary values that must stay valid.
        let ok = with(json!({
            "orphan_watchdog": {"timeout_secs": 90},
            "streaming": {"sse_ping_interval_seconds": 5, "sse_channel_capacity": 64},
            "outbox": {"num_partitions": 64},
            "quota": {"overshoot_tolerance_factor": 1.5, "warning_threshold_pct": 99},
            "knowledge_search": {"enabled": true, "vector_store_id": "v", "provider_id": "p"},
        }));
        load(ok).unwrap();
        load(with_providers(json!({
            "az": provider(json!({"storage_kind": "azure", "api_version": "2025-03-01-preview"})),
            "an": provider(json!({"kind": "anthropic_messages", "rag_provider": "az"})),
            "v6": provider(json!({"host": "[::1]:8080", "port": 8080,
                "tenant_overrides": {"t": {"upstream_alias": "alias-1"}}})),
        })))
        .unwrap();
    }

    #[test]
    fn url_prefix_must_be_a_plain_absolute_path() {
        for bad in [
            "mini-chat",
            "/mini-chat/",
            "/",
            "/{id}",
            "/a}b",
            "/a//b",
            "/a/:b",
            "/a/*rest",
            "/a b",
            "/a?b",
        ] {
            let err = load(with(json!({ "url_prefix": bad }))).expect_err(bad);
            assert!(err.contains("url_prefix"), "{bad}: {err}");
        }
        for ok in ["/mini-chat", "/api/mini-chat", "/v2.chat_x~y-z", ""] {
            assert_eq!(
                load(with(json!({ "url_prefix": ok }))).unwrap().url_prefix,
                ok
            );
        }
    }

    fn auth(secret: &str) -> Value {
        json!({"header": "Authorization", "prefix": "Bearer ", "secret_ref": secret})
    }

    #[test]
    fn upstream_alias_collision_with_different_settings_is_rejected() {
        let collisions: Vec<(&str, Value, [&str; 2])> = vec![
            (
                "two entries on one host with different auth",
                with_providers(json!({
                    "responses": provider(json!({"host": "api.openai.com",
                        "auth_plugin_type": providers::APIKEY_AUTH_PLUGIN,
                        "auth_config": auth("cred://key-a")})),
                    "chat": provider(json!({"kind": "openai_chat_completions",
                        "api_path": "/v1/chat/completions", "host": "API.openai.com",
                        "auth_plugin_type": providers::APIKEY_AUTH_PLUGIN,
                        "auth_config": auth("cred://key-b")})),
                })),
                ["providers.chat", "providers.responses"],
            ),
            (
                "tenant override with the base host and its own auth",
                with_providers(json!({
                    "p": provider(json!({"auth_plugin_type": providers::APIKEY_AUTH_PLUGIN,
                        "auth_config": auth("cred://key-a"),
                        "tenant_overrides": {"t1": {"host": "api.example.com",
                            "auth_config": auth("cred://key-t1")}}})),
                })),
                ["providers.p", "providers.p.tenant_overrides.t1"],
            ),
            (
                "explicit alias shared by two hosts",
                with_providers(json!({
                    "a": provider(json!({"upstream_alias": "shared"})),
                    "b": provider(json!({"host": "other.example.com", "upstream_alias": "Shared"})),
                })),
                ["providers.a", "providers.b"],
            ),
            (
                "explicit alias shared by two ports",
                with_providers(json!({
                    "a": provider(json!({"upstream_alias": "shared", "port": 8443})),
                    "b": provider(json!({"upstream_alias": "shared", "port": 9443})),
                })),
                ["providers.a", "providers.b"],
            ),
            (
                "explicit alias shared by two schemes",
                with_providers(json!({
                    "a": provider(json!({"upstream_alias": "shared"})),
                    "b": provider(json!({"upstream_alias": "shared", "use_http": true})),
                })),
                ["providers.a", "providers.b"],
            ),
            (
                // Every upstream is provisioned under the one S2S context, so overrides of
                // different tenants share the OAGW alias namespace too.
                "overrides of two tenants on one host with different auth",
                with_providers(json!({
                    "p": provider(json!({"tenant_overrides": {
                        "t1": {"host": "eu.example.com", "auth_config": auth("cred://key-t1")},
                        "t2": {"host": "eu.example.com", "auth_config": auth("cred://key-t2")},
                    }})),
                })),
                [
                    "providers.p.tenant_overrides.t1",
                    "providers.p.tenant_overrides.t2",
                ],
            ),
        ];
        for (name, value, labels) in collisions {
            let err = load(value).expect_err(name);
            for label in labels {
                assert!(err.contains(label), "{name}: {err}");
            }
            assert!(err.contains("upstream_alias"), "{name}: {err}");
            assert!(!err.contains("cred://"), "{name} leaks a secret ref: {err}");
        }
    }

    #[test]
    fn upstream_alias_shared_with_identical_settings_is_valid() {
        load(with_providers(json!({
            "responses": provider(json!({"auth_plugin_type": providers::APIKEY_AUTH_PLUGIN,
                "auth_config": auth("cred://key-a"),
                "tenant_overrides": {
                    "t1": {"host": "api.example.com"},
                    "t2": {"host": "eu.example.com", "auth_config": auth("cred://key-eu")},
                    "t3": {"upstream_alias": "t3-alias", "auth_config": auth("cred://key-t3")},
                }})),
            "chat": provider(json!({"kind": "openai_chat_completions",
                "api_path": "/v1/chat/completions",
                "auth_plugin_type": providers::APIKEY_AUTH_PLUGIN,
                "auth_config": auth("cred://key-a"),
                "tenant_overrides": {
                    "t2": {"host": "eu.example.com", "auth_config": auth("cred://key-eu")},
                }})),
            "own-alias": provider(json!({"upstream_alias": "chat-b",
                "auth_config": auth("cred://key-b")})),
        })))
        .unwrap();
    }

    #[test]
    fn config_expands_env_in_host_auth_config_overrides_and_credentials() {
        temp_env::with_vars(
            [
                ("MC_HOST", Some("h.example.com")),
                ("MC_SECRET", Some("s3cret")),
                ("MC_T_HOST", Some("t.example.com")),
                ("MC_ID", Some("cid")),
            ],
            || {
                let cfg = load(json!({
                    "client_credentials": {"client_id": "${MC_ID}", "client_secret": "${MC_SECRET}"},
                    "providers": {"p": provider(json!({
                        "host": "${MC_HOST}",
                        "auth_config": {"secret_ref": "${MC_SECRET}"},
                        "tenant_overrides": {"t": {
                            "host": "${MC_T_HOST}",
                            "auth_config": {"secret_ref": "${MC_SECRET}"}
                        }}
                    }))}
                }))
                .unwrap();
                assert_eq!(cfg.client_credentials.client_id, "cid");
                assert_eq!(cfg.client_credentials.client_secret, "s3cret");
                let p = &cfg.providers["p"];
                assert_eq!(p.host, "h.example.com");
                assert_eq!(p.auth_config.as_ref().unwrap()["secret_ref"], "s3cret");
                let t = &p.tenant_overrides["t"];
                assert_eq!(t.host.as_deref(), Some("t.example.com"));
                assert_eq!(t.auth_config.as_ref().unwrap()["secret_ref"], "s3cret");
            },
        );
    }

    #[test]
    fn estimation_budget_defaults_match_sdk() {
        let sdk = mini_chat_sdk::EstimationBudgets::default();
        let cfg = EstimationBudgetsConfig::default();
        assert_eq!(
            cfg.bytes_per_token_conservative,
            sdk.bytes_per_token_conservative
        );
        assert_eq!(cfg.fixed_overhead_tokens, sdk.fixed_overhead_tokens);
        assert_eq!(cfg.safety_margin_pct, sdk.safety_margin_pct);
        assert_eq!(cfg.image_token_budget, sdk.image_token_budget);
        assert_eq!(cfg.tool_surcharge_tokens, sdk.tool_surcharge_tokens);
        assert_eq!(
            cfg.web_search_surcharge_tokens,
            sdk.web_search_surcharge_tokens
        );
        assert_eq!(
            cfg.code_interpreter_surcharge_tokens,
            sdk.code_interpreter_surcharge_tokens
        );
        assert_eq!(cfg.minimal_generation_floor, sdk.minimal_generation_floor);
    }

    #[test]
    fn client_secret_is_redacted_in_debug() {
        let cfg =
            load(json!({"client_credentials": {"client_id": "id", "client_secret": "topsecret"}}))
                .unwrap();
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("topsecret"), "{dbg}");
        assert!(dbg.contains("id"));
    }

    #[test]
    fn deprecation_warnings_list_non_default_deprecated_fields() {
        let cfg = load(with(json!({
            "estimation_budgets": {"fixed_overhead_tokens": 7, "minimal_generation_floor": 60},
            "cleanup_worker": {"enabled": false, "batch_size": 1},
            "thread_summary_worker": {"reconcile_interval_secs": 5},
        })))
        .unwrap();
        let w = cfg.deprecation_warnings().join("\n");
        assert!(
            w.contains("estimation_budgets.fixed_overhead_tokens"),
            "{w}"
        );
        assert!(!w.contains("minimal_generation_floor"), "{w}");
        assert!(w.contains("cleanup_worker.enabled"), "{w}");
        assert!(w.contains("cleanup_worker.batch_size"), "{w}");
        assert!(!w.contains("cleanup_worker.poll_interval_secs"), "{w}");
        assert!(
            w.contains("thread_summary_worker.reconcile_interval_secs"),
            "{w}"
        );
    }
}
