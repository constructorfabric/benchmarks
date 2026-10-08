//! Mini Chat gear configuration (`gears.mini-chat.config`, DESIGN Appendix B).
//!
//! The top level and the sections `streaming`, `estimation_budgets`, `quota`,
//! `outbox`, `context`, `rag`, `client_credentials`, `metrics`, `providers.<id>`
//! (+ `tenant_overrides.<tenant>`), `thumbnail` and `knowledge_search` reject
//! unknown keys. The worker sections (`orphan_watchdog`, `upload_reaper`,
//! `thread_summary_worker`, `cleanup_worker`) accept and ignore unknown keys.
//! Every section has defaults; [`MiniChatConfig::validate`] runs at gear init.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{Context, bail, ensure};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use toolkit::var_expand::expand_env_vars;
use tracing::warn;

/// Default system prompt of the thread summary request (D B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Default `context.web_search_guard` (P§5.2).
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Default `context.file_search_guard`.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search to look up information in the documents the user attached to this chat when the question refers to them. Base answers about those documents on the retrieved content and do not invent document content.";

/// Default `knowledge_search.guard`.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to look up information in the organization knowledge base when the question concerns internal policies, products or procedures. Base such answers on the retrieved content and say so when nothing relevant is found.";

/// Summary model used when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Default metric prefix when `metrics.prefix` is empty.
pub const DEFAULT_METRICS_PREFIX: &str = "mini_chat";

/// OAGW API-key auth plugin type used by the default `openai` provider.
pub const OAGW_APIKEY_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

// ---------------------------------------------------------------------------
// Top level
// ---------------------------------------------------------------------------

/// Gear configuration root.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiniChatConfig {
    #[serde(default = "default_url_prefix")]
    pub url_prefix: String,
    #[serde(default = "default_vendor")]
    pub vendor: String,
    pub client_credentials: ClientCredentials,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub estimation_budgets: EstimationBudgetsConfig,
    #[serde(default)]
    pub quota: QuotaConfig,
    #[serde(default)]
    pub outbox: OutboxConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub rag: RagConfig,
    #[serde(default)]
    pub thumbnail: ThumbnailConfig,
    #[serde(default)]
    pub orphan_watchdog: OrphanWatchdogConfig,
    #[serde(default)]
    pub upload_reaper: UploadReaperConfig,
    #[serde(default)]
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    #[serde(default)]
    pub cleanup_worker: CleanupWorkerConfig,
    #[serde(default)]
    pub knowledge_search: KnowledgeSearchConfig,
    #[serde(default = "default_providers")]
    pub providers: BTreeMap<String, ProviderEntry>,
}

fn default_url_prefix() -> String {
    "/mini-chat".to_owned()
}

fn default_vendor() -> String {
    "constructorfabric".to_owned()
}

fn default_providers() -> BTreeMap<String, ProviderEntry> {
    let auth_config = BTreeMap::from([
        ("header".to_owned(), "Authorization".to_owned()),
        ("prefix".to_owned(), "Bearer ".to_owned()),
        ("secret_ref".to_owned(), "cred://openai-key".to_owned()),
    ]);
    BTreeMap::from([(
        "openai".to_owned(),
        ProviderEntry {
            kind: ProviderKind::OpenaiResponses,
            host: "api.openai.com".to_owned(),
            port: None,
            use_http: false,
            upstream_alias: None,
            api_path: default_api_path(),
            auth_plugin_type: Some(OAGW_APIKEY_AUTH_PLUGIN.to_owned()),
            auth_config: Some(auth_config),
            storage_kind: StorageKind::Openai,
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: BTreeMap::new(),
        },
    )])
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// S2S credentials exchanged via `authn_resolver`. The secret is redacted in `Debug`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: SecretString,
}

impl ClientCredentials {
    /// The (expanded) client secret.
    #[must_use]
    pub fn secret(&self) -> &str {
        self.client_secret.expose_secret()
    }
}

impl fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .finish()
    }
}

/// Metrics naming.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix; empty means `mini_chat`.
    pub prefix: String,
}

impl MetricsConfig {
    /// The prefix to use (`mini_chat` when unset).
    #[must_use]
    pub fn effective_prefix(&self) -> &str {
        if self.prefix.is_empty() {
            DEFAULT_METRICS_PREFIX
        } else {
            &self.prefix
        }
    }
}

/// SSE streaming (D B.4).
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

/// Gear-level estimation budgets (D B.5.2). Only `minimal_generation_floor`
/// is used; the other fields are deprecated (parsed, warned when non-default).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EstimationBudgetsConfig {
    pub minimal_generation_floor: u32,
    pub bytes_per_token_conservative: u32,
    pub fixed_overhead_tokens: u32,
    pub safety_margin_pct: u32,
    pub image_token_budget: u32,
    pub tool_surcharge_tokens: u32,
    pub web_search_surcharge_tokens: u32,
    pub code_interpreter_surcharge_tokens: u32,
}

impl Default for EstimationBudgetsConfig {
    fn default() -> Self {
        Self {
            minimal_generation_floor: 50,
            bytes_per_token_conservative: 4,
            fixed_overhead_tokens: 100,
            safety_margin_pct: 10,
            image_token_budget: 1000,
            tool_surcharge_tokens: 500,
            web_search_surcharge_tokens: 500,
            code_interpreter_surcharge_tokens: 1000,
        }
    }
}

/// Quota knobs (D B.5.3, B.6, B.6.1).
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

/// Shared outbox queues (D B.9.3).
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

/// Context assembly (D B.5.1).
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

/// Uploads / RAG limits (D B.7, B.8).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RagConfig {
    pub uploaded_file_max_size_kb: u32,
    pub uploaded_image_max_size_kb: u32,
    pub max_images_per_message: u32,
    pub max_documents_per_chat: u32,
    pub max_total_upload_mb_per_chat: u32,
    pub allow_csv_upload: bool,
    pub max_concurrent_uploads: u16,
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            uploaded_file_max_size_kb: 25600,
            uploaded_image_max_size_kb: 5120,
            max_images_per_message: 4,
            max_documents_per_chat: 50,
            max_total_upload_mb_per_chat: 100,
            allow_csv_upload: true,
            max_concurrent_uploads: 10,
        }
    }
}

/// Image thumbnails (D B.8).
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

/// Orphan turn watchdog (D B.9.1). Unknown keys are ignored.
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

/// Upload reaper (D B.9.5). Unknown keys are ignored.
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

/// Thread summary (D B.9.4). Unknown keys are ignored.
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
    /// Max characters per message in the summary prompt; 0 = no truncation.
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

impl ThreadSummaryWorkerConfig {
    /// The summary model id (`gpt-4.1-mini` when unset).
    #[must_use]
    pub fn effective_summary_model_id(&self) -> &str {
        if self.summary_model_id.is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            &self.summary_model_id
        }
    }
}

/// Chat / attachment cleanup (D B.9.2). Unknown keys are ignored.
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

/// Organization knowledge search (D B.7).
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

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// Provider adapter kind (ADR-0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

/// File / vector-store implementation of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

/// One provider entry (`providers.<id>`, D B.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    /// Upstream host; `${VAR}` expanded at validation.
    pub host: String,
    /// Upstream port; see [`ProviderEntry::effective_port`].
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub use_http: bool,
    /// OAGW alias; filled with `host` by [`MiniChatConfig::validate`] when unset.
    #[serde(default)]
    pub upstream_alias: Option<String>,
    /// Chat endpoint path; `{model}` is replaced by `provider_model_id`.
    #[serde(default = "default_api_path")]
    pub api_path: String,
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    /// OAGW auth plugin config; values `${VAR}` expanded at validation.
    #[serde(default)]
    pub auth_config: Option<BTreeMap<String, String>>,
    pub storage_kind: StorageKind,
    /// Label stored with attachments / vector stores; defaults to the provider id.
    #[serde(default)]
    pub storage_backend: Option<String>,
    /// Azure `api-version` (required when `storage_kind = azure`).
    #[serde(default)]
    pub api_version: Option<String>,
    /// Provider used for file / vector-store operations.
    #[serde(default)]
    pub rag_provider: Option<String>,
    /// Per-tenant overrides keyed by tenant id.
    #[serde(default)]
    pub tenant_overrides: BTreeMap<String, TenantOverride>,
}

impl ProviderEntry {
    /// Port to use: explicit `port`, else 80 with `use_http`, else 443.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }

    /// Storage backend label (`storage_backend`, else the provider id).
    #[must_use]
    pub fn effective_storage_backend<'a>(&'a self, provider_id: &'a str) -> &'a str {
        self.storage_backend.as_deref().unwrap_or(provider_id)
    }
}

/// Per-tenant provider override (`providers.<id>.tenant_overrides.<tenant>`).
/// Each unset field falls back to the provider entry.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantOverride {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub upstream_alias: Option<String>,
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    #[serde(default)]
    pub auth_config: Option<BTreeMap<String, String>>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn expand(field: &str, value: &str) -> anyhow::Result<String> {
    expand_env_vars(value).map_err(|e| anyhow::anyhow!("{field}: {e}"))
}

fn expand_map(field: &str, map: &mut Option<BTreeMap<String, String>>) -> anyhow::Result<()> {
    if let Some(map) = map {
        for (k, v) in map.iter_mut() {
            *v = expand(&format!("{field}.{k}"), v)?;
        }
    }
    Ok(())
}

fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

fn is_valid_api_version(v: &str) -> bool {
    !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

fn check_range<T>(field: &str, value: T, min: T, max: T) -> anyhow::Result<()>
where
    T: PartialOrd + fmt::Display + Copy,
{
    ensure!(
        value >= min && value <= max,
        "{field} must be in {min}..={max}, got {value}"
    );
    Ok(())
}

fn check_positive<T>(field: &str, value: T) -> anyhow::Result<()>
where
    T: PartialOrd + Default + fmt::Display + Copy,
{
    ensure!(value > T::default(), "{field} must be > 0, got {value}");
    Ok(())
}

impl MiniChatConfig {
    /// Expand `${VAR}` placeholders, fill defaults derived from other fields
    /// (`upstream_alias`), check ranges, charsets and references, and log a
    /// warning for each deprecated field set to a non-default value.
    ///
    /// # Errors
    ///
    /// Returns an error describing the first invalid value.
    pub fn validate(&mut self) -> anyhow::Result<()> {
        ensure!(!self.vendor.trim().is_empty(), "vendor must be non-empty");
        self.validate_client_credentials()?;
        self.validate_streaming_and_budgets()?;
        self.validate_quota()?;
        self.validate_outbox()?;
        self.validate_context_and_rag()?;
        self.validate_workers()?;
        self.validate_knowledge_search()?;
        self.validate_providers()?;
        self.warn_deprecated();
        Ok(())
    }

    fn validate_client_credentials(&mut self) -> anyhow::Result<()> {
        let cc = &mut self.client_credentials;
        cc.client_id = expand("client_credentials.client_id", &cc.client_id)?;
        let secret = expand(
            "client_credentials.client_secret",
            cc.client_secret.expose_secret(),
        )?;
        cc.client_secret = SecretString::from(secret);
        ensure!(
            !cc.client_id.trim().is_empty(),
            "client_credentials.client_id must be non-empty"
        );
        ensure!(
            !cc.client_secret.expose_secret().trim().is_empty(),
            "client_credentials.client_secret must be non-empty"
        );
        Ok(())
    }

    fn validate_streaming_and_budgets(&self) -> anyhow::Result<()> {
        let s = &self.streaming;
        check_range(
            "streaming.sse_ping_interval_seconds",
            s.sse_ping_interval_seconds,
            5,
            60,
        )?;
        check_range(
            "streaming.sse_channel_capacity",
            s.sse_channel_capacity,
            16,
            64,
        )?;
        check_positive("streaming.max_output_tokens", s.max_output_tokens)?;
        let floor = self.estimation_budgets.minimal_generation_floor;
        check_positive("estimation_budgets.minimal_generation_floor", floor)?;
        ensure!(
            floor <= s.max_output_tokens,
            "estimation_budgets.minimal_generation_floor ({floor}) must be <= streaming.max_output_tokens ({})",
            s.max_output_tokens
        );
        Ok(())
    }

    fn validate_quota(&self) -> anyhow::Result<()> {
        let q = &self.quota;
        ensure!(
            q.overshoot_tolerance_factor.is_finite()
                && (1.0..=1.5).contains(&q.overshoot_tolerance_factor),
            "quota.overshoot_tolerance_factor must be in 1.0..=1.5, got {}",
            q.overshoot_tolerance_factor
        );
        check_range(
            "quota.warning_threshold_pct",
            q.warning_threshold_pct,
            1,
            99,
        )?;
        check_positive(
            "quota.web_search_max_calls_per_message",
            q.web_search_max_calls_per_message,
        )?;
        check_positive("quota.web_search_daily_quota", q.web_search_daily_quota)?;
        check_positive(
            "quota.code_interpreter_max_calls_per_message",
            q.code_interpreter_max_calls_per_message,
        )?;
        check_positive(
            "quota.code_interpreter_daily_quota",
            q.code_interpreter_daily_quota,
        )?;
        Ok(())
    }

    fn validate_outbox(&self) -> anyhow::Result<()> {
        let o = &self.outbox;
        for (field, name) in [
            ("outbox.queue_name", &o.queue_name),
            ("outbox.cleanup_queue_name", &o.cleanup_queue_name),
            ("outbox.chat_cleanup_queue_name", &o.chat_cleanup_queue_name),
            (
                "outbox.thread_summary_queue_name",
                &o.thread_summary_queue_name,
            ),
            ("outbox.audit_queue_name", &o.audit_queue_name),
        ] {
            ensure!(!name.trim().is_empty(), "{field} must be non-empty");
        }
        ensure!(
            o.num_partitions.is_power_of_two() && o.num_partitions <= 64,
            "outbox.num_partitions must be a power of 2 in 1..=64, got {}",
            o.num_partitions
        );
        Ok(())
    }

    fn validate_context_and_rag(&self) -> anyhow::Result<()> {
        check_range(
            "context.recent_messages_limit",
            self.context.recent_messages_limit,
            0,
            100,
        )?;
        let r = &self.rag;
        check_positive("rag.uploaded_file_max_size_kb", r.uploaded_file_max_size_kb)?;
        check_positive(
            "rag.uploaded_image_max_size_kb",
            r.uploaded_image_max_size_kb,
        )?;
        check_positive("rag.max_images_per_message", r.max_images_per_message)?;
        check_positive("rag.max_documents_per_chat", r.max_documents_per_chat)?;
        check_positive(
            "rag.max_total_upload_mb_per_chat",
            r.max_total_upload_mb_per_chat,
        )?;
        check_range(
            "rag.max_concurrent_uploads",
            r.max_concurrent_uploads,
            1,
            256,
        )?;
        let t = &self.thumbnail;
        check_positive("thumbnail.width", t.width)?;
        check_positive("thumbnail.height", t.height)?;
        check_positive("thumbnail.max_bytes", t.max_bytes)?;
        check_positive("thumbnail.max_pixels", t.max_pixels)?;
        check_positive("thumbnail.max_decode_bytes", t.max_decode_bytes)?;
        Ok(())
    }

    fn validate_workers(&self) -> anyhow::Result<()> {
        let w = &self.orphan_watchdog;
        check_range("orphan_watchdog.timeout_secs", w.timeout_secs, 90, 3600)?;
        check_range(
            "orphan_watchdog.scan_interval_secs",
            w.scan_interval_secs,
            1,
            3600,
        )?;
        let r = &self.upload_reaper;
        check_range(
            "upload_reaper.scan_interval_secs",
            r.scan_interval_secs,
            1,
            3600,
        )?;
        check_range(
            "upload_reaper.stale_after_secs",
            r.stale_after_secs,
            60,
            86_400,
        )?;
        let s = &self.thread_summary_worker;
        check_range(
            "thread_summary_worker.claim_timeout_secs",
            s.claim_timeout_secs,
            30,
            3600,
        )?;
        check_positive("thread_summary_worker.max_attempts", s.max_attempts)?;
        check_range(
            "thread_summary_worker.compression_threshold_pct",
            s.compression_threshold_pct,
            1,
            99,
        )?;
        check_positive(
            "cleanup_worker.max_attempts",
            self.cleanup_worker.max_attempts,
        )?;
        Ok(())
    }

    fn validate_knowledge_search(&self) -> anyhow::Result<()> {
        let k = &self.knowledge_search;
        if k.enabled {
            ensure!(
                k.vector_store_id
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty()),
                "knowledge_search.vector_store_id is required when knowledge_search.enabled"
            );
            ensure!(
                k.provider_id
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty()),
                "knowledge_search.provider_id is required when knowledge_search.enabled"
            );
        }
        check_positive(
            "knowledge_search.max_calls_per_message",
            k.max_calls_per_message,
        )?;
        check_positive("knowledge_search.top_k", k.top_k)?;
        check_positive("knowledge_search.max_chunk_chars", k.max_chunk_chars)?;
        Ok(())
    }

    fn validate_providers(&mut self) -> anyhow::Result<()> {
        let ids: Vec<String> = self.providers.keys().cloned().collect();
        for (id, p) in &mut self.providers {
            validate_provider(id, p, &ids)
                .with_context(|| format!("invalid provider entry providers.{id}"))?;
        }
        warn_shared_aliases(&self.providers);
        Ok(())
    }

    fn warn_deprecated(&self) {
        let d = EstimationBudgetsConfig::default();
        let b = &self.estimation_budgets;
        for (field, value, default) in [
            (
                "estimation_budgets.bytes_per_token_conservative",
                b.bytes_per_token_conservative,
                d.bytes_per_token_conservative,
            ),
            (
                "estimation_budgets.fixed_overhead_tokens",
                b.fixed_overhead_tokens,
                d.fixed_overhead_tokens,
            ),
            (
                "estimation_budgets.safety_margin_pct",
                b.safety_margin_pct,
                d.safety_margin_pct,
            ),
            (
                "estimation_budgets.image_token_budget",
                b.image_token_budget,
                d.image_token_budget,
            ),
            (
                "estimation_budgets.tool_surcharge_tokens",
                b.tool_surcharge_tokens,
                d.tool_surcharge_tokens,
            ),
            (
                "estimation_budgets.web_search_surcharge_tokens",
                b.web_search_surcharge_tokens,
                d.web_search_surcharge_tokens,
            ),
            (
                "estimation_budgets.code_interpreter_surcharge_tokens",
                b.code_interpreter_surcharge_tokens,
                d.code_interpreter_surcharge_tokens,
            ),
        ] {
            warn_if_non_default(field, u64::from(value), u64::from(default));
        }

        warn_if_non_default(
            "thread_summary_worker.reconcile_interval_secs",
            self.thread_summary_worker.reconcile_interval_secs,
            ThreadSummaryWorkerConfig::default().reconcile_interval_secs,
        );

        let c = &self.cleanup_worker;
        let cd = CleanupWorkerConfig::default();
        warn_if_non_default(
            "cleanup_worker.enabled",
            u64::from(c.enabled),
            u64::from(cd.enabled),
        );
        for (field, value, default) in [
            (
                "cleanup_worker.poll_interval_secs",
                c.poll_interval_secs,
                cd.poll_interval_secs,
            ),
            (
                "cleanup_worker.reconcile_interval_secs",
                c.reconcile_interval_secs,
                cd.reconcile_interval_secs,
            ),
            (
                "cleanup_worker.stale_in_progress_timeout_secs",
                c.stale_in_progress_timeout_secs,
                cd.stale_in_progress_timeout_secs,
            ),
            (
                "cleanup_worker.batch_size",
                u64::from(c.batch_size),
                u64::from(cd.batch_size),
            ),
        ] {
            warn_if_non_default(field, value, default);
        }
    }
}

/// Log a startup warning for a deprecated field set to a non-default value.
fn warn_if_non_default(field: &str, value: u64, default: u64) {
    if value != default {
        warn!(
            field,
            value,
            default,
            "deprecated mini-chat config field set to a non-default value; it has no effect"
        );
    }
}

fn validate_provider(id: &str, p: &mut ProviderEntry, ids: &[String]) -> anyhow::Result<()> {
    p.host = expand("host", &p.host)?;
    ensure!(
        is_valid_host(&p.host),
        "host must be non-empty and contain only letters, digits, '.', '-', '_', ':', '[', ']' (got {:?})",
        p.host
    );
    expand_map("auth_config", &mut p.auth_config)?;
    ensure!(p.effective_port() != 0, "port must not be 0");
    p.upstream_alias = Some(effective_alias(
        &format!("providers.{id}.upstream_alias"),
        &p.host,
        p.effective_port(),
        p.use_http,
        p.upstream_alias.as_deref(),
    ));
    if p.storage_kind == StorageKind::Azure {
        match p.api_version.as_deref() {
            Some(v) if is_valid_api_version(v) => {}
            Some(v) if !v.trim().is_empty() => {
                bail!("api_version may contain only letters, digits, '.' and '-' (got {v:?})")
            }
            _ => bail!("api_version is required when storage_kind = azure"),
        }
    }
    if let Some(rag) = p.rag_provider.as_deref() {
        ensure!(
            ids.iter().any(|known| known == rag),
            "rag_provider {rag:?} does not name a configured provider"
        );
    }
    let (host, port, use_http) = (p.host.clone(), p.effective_port(), p.use_http);
    for (tenant, o) in &mut p.tenant_overrides {
        if uuid::Uuid::parse_str(tenant).is_err() {
            warn!(
                provider = id,
                tenant = %tenant,
                "tenant_overrides key is not a UUID; it will never match a tenant"
            );
        }
        validate_override(o).with_context(|| format!("invalid tenant_overrides.{tenant}"))?;
        o.upstream_alias = Some(effective_alias(
            &format!("providers.{id}.tenant_overrides.{tenant}.upstream_alias"),
            o.host.as_deref().unwrap_or(&host),
            port,
            use_http,
            o.upstream_alias.as_deref(),
        ));
    }
    Ok(())
}

fn validate_override(o: &mut TenantOverride) -> anyhow::Result<()> {
    if let Some(host) = o.host.as_mut() {
        *host = expand("host", host)?;
        ensure!(
            is_valid_host(host),
            "host must be non-empty and contain only letters, digits, '.', '-', '_', ':', '[', ']' (got {host:?})"
        );
    }
    expand_map("auth_config", &mut o.auth_config)?;
    if o.upstream_alias.as_deref().is_none_or(str::is_empty) {
        o.upstream_alias = None;
    }
    ensure!(
        o.host.is_some() || o.upstream_alias.is_some(),
        "an override must set host or upstream_alias"
    );
    Ok(())
}

/// OAGW's derived alias of a single-endpoint upstream (mirrors oagw
/// `Endpoint::alias_contribution`): the normalized host (brackets stripped,
/// lowercase, trailing dots trimmed), with `:port` unless the port is the
/// scheme's standard port (80 for http, 443 for https).
#[must_use]
pub fn oagw_derived_alias(host: &str, port: u16, use_http: bool) -> String {
    let host = normalized_host(host);
    let standard = if use_http { port == 80 } else { port == 443 };
    if standard {
        host
    } else {
        format!("{host}:{port}")
    }
}

fn normalized_host(host: &str) -> String {
    let h = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    normalize_alias(h)
}

/// OAGW alias normalization (lowercase, trailing dots trimmed).
fn normalize_alias(alias: &str) -> String {
    alias.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// The alias OAGW registers the upstream under: an unset alias is OAGW's
/// derivation; an explicit alias is kept for IP hosts (OAGW requires one
/// there) and must equal the derivation for hostnames, otherwise OAGW would
/// reject it — it is then replaced by the derived alias with a warning.
fn effective_alias(
    field: &str,
    host: &str,
    port: u16,
    use_http: bool,
    explicit: Option<&str>,
) -> String {
    let derived = oagw_derived_alias(host, port, use_http);
    let Some(explicit) = explicit.filter(|a| !a.trim().is_empty()) else {
        return derived;
    };
    let normalized = normalize_alias(explicit);
    if normalized_host(host).parse::<std::net::IpAddr>().is_ok() || normalized == derived {
        return normalized;
    }
    warn!(
        field,
        configured = explicit,
        alias = %derived,
        "upstream_alias differs from the alias OAGW derives for this hostname endpoint; using the derived alias"
    );
    derived
}

/// What one OAGW upstream is made of (for shared-alias detection).
#[derive(PartialEq)]
struct UpstreamIdentity<'a> {
    use_http: bool,
    host: String,
    port: u16,
    auth_plugin_type: Option<&'a String>,
    auth_config: Option<&'a BTreeMap<String, String>>,
}

/// Two entries / overrides that resolve to the same OAGW alias with a
/// different endpoint or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasConflict {
    pub alias: String,
    /// Label of the entry that owns the upstream (first in order).
    pub owner: String,
    /// Label of the entry that reuses it.
    pub other: String,
}

/// Entries sharing an OAGW alias with a different endpoint or credentials.
///
/// D B.1: the alias is always passed to OAGW, which creates or reuses the
/// upstream under it, so such configurations are valid. OAGW holds one
/// upstream per alias: the first entry in deterministic order (provider ids
/// ascending, each entry before its tenant overrides in tenant order) owns
/// the upstream's endpoint and credentials and the others reuse it (see
/// `oagw_provisioning::plan`). Labels are `{id}` / `{id}.tenant_overrides.{tenant}`.
#[must_use]
pub fn shared_alias_conflicts(providers: &BTreeMap<String, ProviderEntry>) -> Vec<AliasConflict> {
    let mut seen: BTreeMap<String, (String, UpstreamIdentity<'_>)> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for (id, p) in providers {
        let base = UpstreamIdentity {
            use_http: p.use_http,
            host: normalized_host(&p.host),
            port: p.effective_port(),
            auth_plugin_type: p.auth_plugin_type.as_ref(),
            auth_config: p.auth_config.as_ref(),
        };
        let mut items = vec![(id.clone(), p.upstream_alias.clone(), base)];
        for (tenant, o) in &p.tenant_overrides {
            items.push((
                format!("{id}.tenant_overrides.{tenant}"),
                o.upstream_alias.clone(),
                UpstreamIdentity {
                    use_http: p.use_http,
                    host: normalized_host(o.host.as_deref().unwrap_or(&p.host)),
                    port: p.effective_port(),
                    auth_plugin_type: o.auth_plugin_type.as_ref().or(p.auth_plugin_type.as_ref()),
                    auth_config: o.auth_config.as_ref().or(p.auth_config.as_ref()),
                },
            ));
        }
        for (label, alias, identity) in items {
            let alias = alias.unwrap_or_default();
            match seen.get(&alias) {
                Some((owner, known)) if *known != identity => conflicts.push(AliasConflict {
                    alias: alias.clone(),
                    owner: owner.clone(),
                    other: label,
                }),
                Some(_) => {}
                None => {
                    seen.insert(alias, (label, identity));
                }
            }
        }
    }
    conflicts
}

/// Log one WARN per [`shared_alias_conflicts`] entry (never fails startup).
fn warn_shared_aliases(providers: &BTreeMap<String, ProviderEntry>) {
    for c in shared_alias_conflicts(providers) {
        warn!(
            alias = %c.alias,
            owner = %format!("providers.{}", c.owner),
            other = %format!("providers.{}", c.other),
            "provider entries share an OAGW upstream alias with a different endpoint or credentials; \
             OAGW reuses one upstream, provisioned from the owner's endpoint and credentials"
        );
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
