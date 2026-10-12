//! Mini-chat gear configuration (DESIGN Appendix B).
//!
//! The configuration is loaded from the gear's config section at init, then
//! prepared in three steps before any service is built:
//!
//! 1. [`MiniChatConfig::expand_env`] expands `${VAR}` placeholders in provider
//!    `host` and `auth_config` values (including tenant overrides) and in
//!    `client_credentials` (B.1);
//! 2. [`MiniChatConfig::fill_upstream_aliases`] fills every unset
//!    `upstream_alias` (B.1 provider table);
//! 3. [`MiniChatConfig::validate`] checks every section; an invalid value
//!    fails startup.
//!
//! Unknown keys are rejected at the top level and in every section except the
//! four worker sections (`orphan_watchdog`, `upload_reaper`,
//! `thread_summary_worker`, `cleanup_worker`), which accept and ignore them.
//! Deprecated keys set to a non-default value are reported by
//! [`MiniChatConfig::deprecation_warnings`] (B.5.2, B.9.2, B.9.4).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Display;
use std::ops::RangeInclusive;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use toolkit::var_expand::expand_env_vars;

// ── Built-in texts and well-known defaults ──────────────────────────────

/// Built-in `context.web_search_guard` (DESIGN section 4 "Web Search",
/// system prompt constraint).
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Built-in `context.file_search_guard`, appended to the system prompt when the
/// `file_search` tool is sent.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search to look up information in the documents attached to this chat when the question concerns their content. Base your answer on the retrieved passages. If the documents do not contain the answer, say so instead of guessing.";

/// Built-in `knowledge_search.guard`, appended to the system prompt when the
/// `search_knowledge` tool is sent.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to look up information in the knowledge base when the question needs facts that are not in the conversation. Base your answer on the returned passages and do not invent facts they do not contain. If the knowledge base has no relevant information, say so.";

/// Built-in summary system prompt (DESIGN B.5.5); also the default of
/// `thread_summary_worker.summary_system_prompt`.
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Catalog model used for thread summaries when
/// `thread_summary_worker.summary_model_id` is empty (B.9.4).
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Metric name prefix used when `metrics.prefix` is empty (B.1).
pub const DEFAULT_METRICS_PREFIX: &str = "mini_chat";

/// OAGW API-key auth plugin type of the default `openai` provider entry.
pub const APIKEY_AUTH_PLUGIN_TYPE: &str = oagw_sdk::gts::APIKEY_AUTH_PLUGIN_ID;

const DEFAULT_PROVIDER_ID: &str = "openai";
const DEFAULT_PROVIDER_HOST: &str = "api.openai.com";
const DEFAULT_PROVIDER_SECRET_REF: &str = "cred://openai-key";
const DEFAULT_API_PATH: &str = "/v1/responses";
const HTTPS_PORT: u16 = 443;
const HTTP_PORT: u16 = 80;

// ── Top level ────────────────────────────────────────────────────────────

/// Gear configuration (`gears.mini-chat.config`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    /// Prefix of all REST routes.
    pub url_prefix: String,
    /// Vendor selecting the model-policy and audit plugin instances.
    pub vendor: String,
    /// S2S credentials exchanged via `authn_resolver` for OAGW provisioning.
    pub client_credentials: ClientCredentials,
    pub metrics: MetricsConfig,
    /// Provider registry keyed by provider id (`ModelCatalogEntry.provider_id`).
    pub providers: BTreeMap<String, ProviderEntry>,
    pub streaming: StreamingConfig,
    pub estimation_budgets: EstimationBudgetsConfig,
    pub quota: QuotaConfig,
    pub outbox: OutboxConfig,
    pub context: ContextConfig,
    pub rag: RagConfig,
    pub thumbnail: ThumbnailConfig,
    pub knowledge_search: KnowledgeSearchConfig,
    pub orphan_watchdog: OrphanWatchdogConfig,
    pub upload_reaper: UploadReaperConfig,
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    pub cleanup_worker: CleanupWorkerConfig,
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
            knowledge_search: KnowledgeSearchConfig::default(),
            orphan_watchdog: OrphanWatchdogConfig::default(),
            upload_reaper: UploadReaperConfig::default(),
            thread_summary_worker: ThreadSummaryWorkerConfig::default(),
            cleanup_worker: CleanupWorkerConfig::default(),
        }
    }
}

fn default_providers() -> BTreeMap<String, ProviderEntry> {
    let auth_config = HashMap::from([
        ("header".to_owned(), "Authorization".to_owned()),
        ("prefix".to_owned(), "Bearer ".to_owned()),
        (
            "secret_ref".to_owned(),
            DEFAULT_PROVIDER_SECRET_REF.to_owned(),
        ),
    ]);
    let openai = ProviderEntry {
        kind: ProviderKind::OpenaiResponses,
        host: DEFAULT_PROVIDER_HOST.to_owned(),
        port: None,
        use_http: false,
        upstream_alias: None,
        api_path: default_api_path(),
        auth_plugin_type: Some(APIKEY_AUTH_PLUGIN_TYPE.to_owned()),
        auth_config: Some(auth_config),
        storage_kind: StorageKind::Openai,
        storage_backend: None,
        api_version: None,
        rag_provider: None,
        tenant_overrides: BTreeMap::new(),
    };
    BTreeMap::from([(DEFAULT_PROVIDER_ID.to_owned(), openai)])
}

// ── Credentials and metrics ──────────────────────────────────────────────

/// S2S client credentials (required; both values must be non-empty).
///
/// The secret is a [`SecretString`]: `Debug` prints it redacted.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: SecretString,
}

impl Default for ClientCredentials {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: SecretString::from(String::new()),
        }
    }
}

/// Metrics settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix; empty means [`DEFAULT_METRICS_PREFIX`].
    pub prefix: String,
}

impl MetricsConfig {
    /// Prefix applied to metric names.
    #[must_use]
    pub fn effective_prefix(&self) -> &str {
        if self.prefix.is_empty() {
            DEFAULT_METRICS_PREFIX
        } else {
            &self.prefix
        }
    }
}

// ── Providers ────────────────────────────────────────────────────────────

/// Provider adapter kind (ADR-0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

/// File / vector-store implementation of a provider entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

/// One entry of `providers.<id>` (B.1 provider table).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    /// Upstream host; supports `${VAR}` expansion.
    pub host: String,
    /// Upstream port; `None` means 443 (80 when `use_http`).
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub use_http: bool,
    /// OAGW upstream alias; filled with `host` by
    /// [`MiniChatConfig::fill_upstream_aliases`] when unset.
    #[serde(default)]
    pub upstream_alias: Option<String>,
    /// Chat endpoint path; `{model}` is replaced by `provider_model_id`.
    #[serde(default = "default_api_path")]
    pub api_path: String,
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    /// OAGW auth plugin config (`header`, `prefix`, `secret_ref`, ...);
    /// values support `${VAR}` expansion.
    #[serde(default)]
    pub auth_config: Option<HashMap<String, String>>,
    pub storage_kind: StorageKind,
    /// Storage backend label; `None` means the provider id.
    #[serde(default)]
    pub storage_backend: Option<String>,
    /// Azure `api-version` for RAG requests; required for `storage_kind = azure`.
    #[serde(default)]
    pub api_version: Option<String>,
    /// Provider used for file / vector-store operations.
    #[serde(default)]
    pub rag_provider: Option<String>,
    /// Per-tenant overrides keyed by tenant id.
    #[serde(default)]
    pub tenant_overrides: BTreeMap<String, TenantOverride>,
}

fn default_api_path() -> String {
    DEFAULT_API_PATH.to_owned()
}

impl ProviderEntry {
    /// Upstream port: the configured one, or 443 (80 when `use_http`).
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        match self.port {
            Some(port) => port,
            None if self.use_http => HTTP_PORT,
            None => HTTPS_PORT,
        }
    }

    /// Storage backend label: `storage_backend`, or the provider id.
    #[must_use]
    pub fn effective_storage_backend<'a>(&'a self, provider_id: &'a str) -> &'a str {
        self.storage_backend.as_deref().unwrap_or(provider_id)
    }
}

/// `providers.<id>.tenant_overrides.<tenant_id>`: each field falls back to
/// the provider entry on its own; `host` or `upstream_alias` must be set.
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
    pub auth_config: Option<HashMap<String, String>>,
}

// ── Streaming, budgets, quota ────────────────────────────────────────────

/// SSE streaming settings (B.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    pub sse_ping_interval_seconds: u16,
    pub sse_channel_capacity: u16,
    /// Cap on `max_output_tokens_applied`.
    pub max_output_tokens: u32,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            sse_ping_interval_seconds: 15,
            sse_channel_capacity: 32,
            max_output_tokens: 32_768,
        }
    }
}

/// Gear-level estimation budgets (B.5.2). Only `minimal_generation_floor` is
/// used; the other fields are deprecated (parsed, not validated, warned about
/// when set to a non-default value).
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

/// Quota knobs (B.5.3, B.6, B.6.1).
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

// ── Outbox, context, RAG, thumbnails, knowledge search ───────────────────

/// Shared outbox queues (B.9.3).
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

/// Context assembly settings (B.5.1).
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

/// Upload and per-chat document limits (B.7, B.8).
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
            uploaded_file_max_size_kb: 25_600,
            uploaded_image_max_size_kb: 5_120,
            max_images_per_message: 4,
        }
    }
}

/// Image thumbnail generation (B.8).
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

/// Knowledge search (B.7 "Knowledge search").
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

// ── Background workers (unknown keys accepted) ───────────────────────────

/// Orphan watchdog (B.9.1).
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

/// Upload reaper (B.9.5).
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

/// Thread summary worker (B.9.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThreadSummaryWorkerConfig {
    pub enabled: bool,
    pub claim_timeout_secs: u64,
    pub max_attempts: u32,
    pub compression_threshold_pct: u32,
    /// Catalog model for summaries; empty means [`DEFAULT_SUMMARY_MODEL_ID`].
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
    /// Catalog model id used for summaries.
    #[must_use]
    pub fn effective_summary_model_id(&self) -> &str {
        if self.summary_model_id.is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            &self.summary_model_id
        }
    }
}

/// Chat-deletion cleanup (B.9.2). Only `max_attempts` has an effect.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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

// ── Preparation: expansion, alias fill, deprecation warnings ─────────────

impl MiniChatConfig {
    /// Expand `${VAR}` / `${VAR:-default}` placeholders in provider `host` and
    /// `auth_config` values, the same fields of every tenant override, and
    /// `client_credentials` (B.1).
    ///
    /// # Errors
    /// Returns a description naming the config key and the missing variable
    /// (never the secret value) when a referenced variable is not set.
    pub fn expand_env(&mut self) -> Result<(), String> {
        let creds = &mut self.client_credentials;
        creds.client_id = expand_value("client_credentials.client_id", &creds.client_id)?;
        let secret = expand_value(
            "client_credentials.client_secret",
            creds.client_secret.expose_secret(),
        )?;
        creds.client_secret = SecretString::from(secret);

        for (id, provider) in &mut self.providers {
            let key = format!("providers.{id}");
            provider.host = expand_value(&format!("{key}.host"), &provider.host)?;
            expand_auth_config(&key, provider.auth_config.as_mut())?;
            for (tenant, entry) in &mut provider.tenant_overrides {
                let key = format!("{key}.tenant_overrides.{tenant}");
                if let Some(host) = entry.host.as_mut() {
                    *host = expand_value(&format!("{key}.host"), host)?;
                }
                expand_auth_config(&key, entry.auth_config.as_mut())?;
            }
        }
        Ok(())
    }

    /// Fill every unset `upstream_alias`: a provider entry gets its `host`, a
    /// tenant override gets the override `host` (B.1). An override without a
    /// `host` keeps no alias (validation rejects it).
    pub fn fill_upstream_aliases(&mut self) {
        for provider in self.providers.values_mut() {
            if provider.upstream_alias.is_none() {
                provider.upstream_alias = Some(provider.host.clone());
            }
            for entry in provider.tenant_overrides.values_mut() {
                if entry.upstream_alias.is_none() {
                    entry.upstream_alias.clone_from(&entry.host);
                }
            }
        }
    }

    /// One message per deprecated key set to a non-default value
    /// (`estimation_budgets.*` except `minimal_generation_floor`,
    /// `cleanup_worker.*` except `max_attempts`,
    /// `thread_summary_worker.reconcile_interval_secs`). Gear init logs each
    /// one as a warning.
    #[must_use]
    pub fn deprecation_warnings(&self) -> Vec<String> {
        let budgets = &self.estimation_budgets;
        let default_budgets = EstimationBudgetsConfig::default();
        let cleanup = &self.cleanup_worker;
        let default_cleanup = CleanupWorkerConfig::default();
        let summary_default = ThreadSummaryWorkerConfig::default();

        let checks: [(&str, u64, u64); 12] = [
            (
                "estimation_budgets.bytes_per_token_conservative",
                budgets.bytes_per_token_conservative.into(),
                default_budgets.bytes_per_token_conservative.into(),
            ),
            (
                "estimation_budgets.fixed_overhead_tokens",
                budgets.fixed_overhead_tokens.into(),
                default_budgets.fixed_overhead_tokens.into(),
            ),
            (
                "estimation_budgets.safety_margin_pct",
                budgets.safety_margin_pct.into(),
                default_budgets.safety_margin_pct.into(),
            ),
            (
                "estimation_budgets.image_token_budget",
                budgets.image_token_budget.into(),
                default_budgets.image_token_budget.into(),
            ),
            (
                "estimation_budgets.tool_surcharge_tokens",
                budgets.tool_surcharge_tokens.into(),
                default_budgets.tool_surcharge_tokens.into(),
            ),
            (
                "estimation_budgets.web_search_surcharge_tokens",
                budgets.web_search_surcharge_tokens.into(),
                default_budgets.web_search_surcharge_tokens.into(),
            ),
            (
                "estimation_budgets.code_interpreter_surcharge_tokens",
                budgets.code_interpreter_surcharge_tokens.into(),
                default_budgets.code_interpreter_surcharge_tokens.into(),
            ),
            (
                "cleanup_worker.poll_interval_secs",
                cleanup.poll_interval_secs,
                default_cleanup.poll_interval_secs,
            ),
            (
                "cleanup_worker.reconcile_interval_secs",
                cleanup.reconcile_interval_secs,
                default_cleanup.reconcile_interval_secs,
            ),
            (
                "cleanup_worker.stale_in_progress_timeout_secs",
                cleanup.stale_in_progress_timeout_secs,
                default_cleanup.stale_in_progress_timeout_secs,
            ),
            (
                "cleanup_worker.batch_size",
                cleanup.batch_size.into(),
                default_cleanup.batch_size.into(),
            ),
            (
                "thread_summary_worker.reconcile_interval_secs",
                self.thread_summary_worker.reconcile_interval_secs,
                summary_default.reconcile_interval_secs,
            ),
        ];

        let mut warnings: Vec<String> = checks
            .iter()
            .filter(|(_, value, default)| value != default)
            .map(|(key, value, _)| deprecated_message(key, value))
            .collect();
        if !cleanup.enabled {
            warnings.push(deprecated_message("cleanup_worker.enabled", &false));
        }
        warnings
    }
}

fn deprecated_message(key: &str, value: &dyn Display) -> String {
    format!("mini-chat config: `{key}` = {value} is deprecated and has no effect")
}

fn expand_value(key: &str, raw: &str) -> Result<String, String> {
    expand_env_vars(raw).map_err(|err| format!("mini-chat config: {key}: {err}"))
}

fn expand_auth_config(
    key: &str,
    auth_config: Option<&mut HashMap<String, String>>,
) -> Result<(), String> {
    let Some(auth_config) = auth_config else {
        return Ok(());
    };
    for (name, value) in auth_config.iter_mut() {
        *value = expand_value(&format!("{key}.auth_config.{name}"), value)?;
    }
    Ok(())
}

// ── Validation ───────────────────────────────────────────────────────────

impl MiniChatConfig {
    /// Validate every section (DESIGN section 3.2 "Gear lifecycle", Appendix B).
    ///
    /// Runs after [`Self::expand_env`]: an unexpanded `${VAR}` host is
    /// rejected by the host character check.
    ///
    /// # Errors
    /// Returns a description of the first invalid value.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_top_level()?;
        self.validate_providers()?;
        self.validate_streaming_and_budgets()?;
        self.validate_quota()?;
        self.validate_outbox()?;
        check_range(
            "context.recent_messages_limit",
            self.context.recent_messages_limit,
            0..=100,
        )?;
        self.validate_rag()?;
        self.validate_thumbnail()?;
        self.validate_knowledge_search()?;
        self.validate_workers()
    }

    fn validate_top_level(&self) -> Result<(), String> {
        check_non_empty("vendor", &self.vendor)?;
        check_non_empty(
            "client_credentials.client_id",
            &self.client_credentials.client_id,
        )?;
        check_non_empty(
            "client_credentials.client_secret",
            self.client_credentials.client_secret.expose_secret(),
        )
    }

    fn validate_providers(&self) -> Result<(), String> {
        for (id, provider) in &self.providers {
            let key = format!("providers.{id}");
            validate_provider(&key, provider)?;
            if let Some(rag_provider) = provider.rag_provider.as_deref()
                && !self.providers.contains_key(rag_provider)
            {
                return Err(format!(
                    "mini-chat config: {key}.rag_provider = {rag_provider:?} does not name a configured provider"
                ));
            }
        }
        Ok(())
    }

    fn validate_streaming_and_budgets(&self) -> Result<(), String> {
        let streaming = &self.streaming;
        check_range(
            "streaming.sse_ping_interval_seconds",
            streaming.sse_ping_interval_seconds,
            5..=60,
        )?;
        check_range(
            "streaming.sse_channel_capacity",
            streaming.sse_channel_capacity,
            16..=64,
        )?;
        check_range(
            "estimation_budgets.minimal_generation_floor",
            self.estimation_budgets.minimal_generation_floor,
            1..=streaming.max_output_tokens,
        )
    }

    fn validate_quota(&self) -> Result<(), String> {
        let quota = &self.quota;
        check_range(
            "quota.overshoot_tolerance_factor",
            quota.overshoot_tolerance_factor,
            1.0..=1.5,
        )?;
        check_range(
            "quota.warning_threshold_pct",
            quota.warning_threshold_pct,
            1..=99,
        )?;
        check_positive(
            "quota.web_search_max_calls_per_message",
            quota.web_search_max_calls_per_message,
        )?;
        check_positive("quota.web_search_daily_quota", quota.web_search_daily_quota)?;
        check_positive(
            "quota.code_interpreter_max_calls_per_message",
            quota.code_interpreter_max_calls_per_message,
        )?;
        check_positive(
            "quota.code_interpreter_daily_quota",
            quota.code_interpreter_daily_quota,
        )
    }

    fn validate_outbox(&self) -> Result<(), String> {
        let outbox = &self.outbox;
        check_non_empty("outbox.queue_name", &outbox.queue_name)?;
        check_non_empty("outbox.cleanup_queue_name", &outbox.cleanup_queue_name)?;
        check_non_empty(
            "outbox.chat_cleanup_queue_name",
            &outbox.chat_cleanup_queue_name,
        )?;
        check_non_empty(
            "outbox.thread_summary_queue_name",
            &outbox.thread_summary_queue_name,
        )?;
        check_non_empty("outbox.audit_queue_name", &outbox.audit_queue_name)?;
        let partitions = outbox.num_partitions;
        if !(partitions.is_power_of_two() && partitions <= 64) {
            return Err(format!(
                "mini-chat config: outbox.num_partitions = {partitions} must be a power of two in 1..=64"
            ));
        }
        Ok(())
    }

    fn validate_rag(&self) -> Result<(), String> {
        let rag = &self.rag;
        check_positive("rag.max_documents_per_chat", rag.max_documents_per_chat)?;
        check_positive(
            "rag.max_total_upload_mb_per_chat",
            rag.max_total_upload_mb_per_chat,
        )?;
        check_range(
            "rag.max_concurrent_uploads",
            rag.max_concurrent_uploads,
            1..=256,
        )?;
        check_positive(
            "rag.uploaded_file_max_size_kb",
            rag.uploaded_file_max_size_kb,
        )?;
        check_positive(
            "rag.uploaded_image_max_size_kb",
            rag.uploaded_image_max_size_kb,
        )?;
        check_positive("rag.max_images_per_message", rag.max_images_per_message)
    }

    fn validate_thumbnail(&self) -> Result<(), String> {
        let thumbnail = &self.thumbnail;
        check_positive("thumbnail.width", thumbnail.width)?;
        check_positive("thumbnail.height", thumbnail.height)?;
        check_positive("thumbnail.max_bytes", thumbnail.max_bytes)?;
        check_positive("thumbnail.max_pixels", thumbnail.max_pixels)?;
        check_positive("thumbnail.max_decode_bytes", thumbnail.max_decode_bytes)
    }

    fn validate_knowledge_search(&self) -> Result<(), String> {
        let knowledge = &self.knowledge_search;
        check_positive(
            "knowledge_search.max_calls_per_message",
            knowledge.max_calls_per_message,
        )?;
        check_positive("knowledge_search.top_k", knowledge.top_k)?;
        check_positive(
            "knowledge_search.max_chunk_chars",
            knowledge.max_chunk_chars,
        )?;
        if knowledge.enabled {
            check_required(
                "knowledge_search.vector_store_id",
                knowledge.vector_store_id.as_deref(),
            )?;
            check_required(
                "knowledge_search.provider_id",
                knowledge.provider_id.as_deref(),
            )?;
        }
        Ok(())
    }

    fn validate_workers(&self) -> Result<(), String> {
        let watchdog = &self.orphan_watchdog;
        check_range(
            "orphan_watchdog.timeout_secs",
            watchdog.timeout_secs,
            90..=3600,
        )?;
        check_range(
            "orphan_watchdog.scan_interval_secs",
            watchdog.scan_interval_secs,
            1..=3600,
        )?;

        let reaper = &self.upload_reaper;
        check_range(
            "upload_reaper.scan_interval_secs",
            reaper.scan_interval_secs,
            1..=3600,
        )?;
        check_range(
            "upload_reaper.stale_after_secs",
            reaper.stale_after_secs,
            60..=86_400,
        )?;

        let summary = &self.thread_summary_worker;
        check_range(
            "thread_summary_worker.claim_timeout_secs",
            summary.claim_timeout_secs,
            30..=3600,
        )?;
        check_positive("thread_summary_worker.max_attempts", summary.max_attempts)?;
        check_range(
            "thread_summary_worker.compression_threshold_pct",
            summary.compression_threshold_pct,
            1..=99,
        )?;

        check_positive(
            "cleanup_worker.max_attempts",
            self.cleanup_worker.max_attempts,
        )
    }
}

fn validate_provider(key: &str, provider: &ProviderEntry) -> Result<(), String> {
    check_host(&format!("{key}.host"), &provider.host)?;
    if provider.port == Some(0) {
        return Err(format!("mini-chat config: {key}.port must not be 0"));
    }
    check_optional_non_blank(
        &format!("{key}.upstream_alias"),
        provider.upstream_alias.as_deref(),
    )?;
    check_api_version(key, provider)?;
    for (tenant, entry) in &provider.tenant_overrides {
        validate_tenant_override(&format!("{key}.tenant_overrides.{tenant}"), entry)?;
    }
    Ok(())
}

fn check_api_version(key: &str, provider: &ProviderEntry) -> Result<(), String> {
    let version = provider.api_version.as_deref().unwrap_or_default();
    if version.trim().is_empty() {
        if provider.storage_kind == StorageKind::Azure {
            return Err(format!(
                "mini-chat config: {key}.api_version is required when storage_kind = azure"
            ));
        }
        return Ok(());
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '-';
    if !version.chars().all(allowed) {
        return Err(format!(
            "mini-chat config: {key}.api_version = {version:?} may contain only letters, digits, '.' and '-'"
        ));
    }
    Ok(())
}

fn validate_tenant_override(key: &str, entry: &TenantOverride) -> Result<(), String> {
    if entry.host.is_none() && entry.upstream_alias.is_none() {
        return Err(format!(
            "mini-chat config: {key} must set `host` or `upstream_alias`"
        ));
    }
    if let Some(host) = entry.host.as_deref() {
        check_host(&format!("{key}.host"), host)?;
    }
    check_optional_non_blank(
        &format!("{key}.upstream_alias"),
        entry.upstream_alias.as_deref(),
    )
}

/// A host is the OAGW alias in `/{alias}/...`: only letters, digits, `.`,
/// `-`, `_`, `:`, `[`, `]` are allowed (B.1).
fn check_host(key: &str, host: &str) -> Result<(), String> {
    let allowed =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']');
    if host.is_empty() || !host.chars().all(allowed) {
        return Err(format!(
            "mini-chat config: {key} = {host:?} must be non-empty and contain only letters, digits, '.', '-', '_', ':', '[', ']' (unexpanded ${{VAR}}?)"
        ));
    }
    Ok(())
}

fn check_non_empty(key: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("mini-chat config: {key} must be non-empty"));
    }
    Ok(())
}

fn check_required(key: &str, value: Option<&str>) -> Result<(), String> {
    check_non_empty(key, value.unwrap_or_default()).map_err(|_| {
        format!("mini-chat config: {key} is required when knowledge_search.enabled = true")
    })
}

fn check_optional_non_blank(key: &str, value: Option<&str>) -> Result<(), String> {
    value.map_or(Ok(()), |value| check_non_empty(key, value))
}

fn check_range<T>(key: &str, value: T, range: RangeInclusive<T>) -> Result<(), String>
where
    T: PartialOrd + Display + Copy,
{
    if range.contains(&value) {
        return Ok(());
    }
    Err(format!(
        "mini-chat config: {key} = {value} is out of range {}..={}",
        range.start(),
        range.end()
    ))
}

fn check_positive<T>(key: &str, value: T) -> Result<(), String>
where
    T: PartialOrd + Display + Default + Copy,
{
    if value > T::default() {
        return Ok(());
    }
    Err(format!("mini-chat config: {key} = {value} must be > 0"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::MiniChatConfig;

    fn config(raw: serde_json::Value) -> MiniChatConfig {
        serde_json::from_value(raw).unwrap_or_else(|err| panic!("config: {err}"))
    }

    fn with_tenant_override() -> MiniChatConfig {
        config(json!({
            "client_credentials": { "client_id": "${MC_CFG_ID}", "client_secret": "${MC_CFG_SECRET}" },
            "providers": { "azure": {
                "kind": "openai_responses", "host": "${MC_CFG_HOST}", "storage_kind": "azure",
                "api_version": "2025-03-01-preview",
                "auth_config": { "secret_ref": "${MC_CFG_REF:-default-ref}" },
                "tenant_overrides": {
                    "t1": { "host": "${MC_CFG_TENANT_HOST}" },
                    "t2": { "upstream_alias": "explicit" }
                }
            }}
        }))
    }

    #[test]
    fn expand_env_covers_hosts_auth_config_and_credentials() {
        let vars = [
            ("MC_CFG_ID", Some("id")),
            ("MC_CFG_SECRET", Some("s3cr3t")),
            ("MC_CFG_HOST", Some("res.openai.azure.com")),
            ("MC_CFG_TENANT_HOST", Some("tenant.openai.azure.com")),
            ("MC_CFG_REF", None),
        ];
        temp_env::with_vars(vars, || {
            let mut cfg = with_tenant_override();
            assert_eq!(cfg.expand_env(), Ok(()));
            cfg.fill_upstream_aliases();
            assert_eq!(cfg.validate(), Ok(()));

            let p = &cfg.providers["azure"];
            let got = [
                Some(p.host.as_str()),
                p.upstream_alias.as_deref(),
                p.auth_config
                    .as_ref()
                    .and_then(|a| a.get("secret_ref"))
                    .map(String::as_str),
                p.tenant_overrides["t1"].upstream_alias.as_deref(),
                p.tenant_overrides["t2"].upstream_alias.as_deref(),
                Some(cfg.client_credentials.client_id.as_str()),
            ];
            let host = "res.openai.azure.com";
            let tenant = "tenant.openai.azure.com";
            let want = [host, host, "default-ref", tenant, "explicit", "id"].map(Some);
            assert_eq!(got, want);
            assert!(
                !format!("{cfg:?}").contains("s3cr3t"),
                "secret must be redacted"
            );
        });
    }

    #[test]
    fn expand_env_error_names_the_key_but_not_the_secret() {
        temp_env::with_vars([("MC_CFG_MISSING", None::<&str>)], || {
            let mut cfg = config(json!({
                "client_credentials": { "client_id": "a", "client_secret": "top-${MC_CFG_MISSING}" }
            }));
            let err = cfg.expand_env().err().unwrap_or_default();
            assert!(err.contains("client_credentials.client_secret"), "{err}");
            assert!(err.contains("MC_CFG_MISSING"), "{err}");
            assert!(!err.contains("top-"), "{err}");
        });
    }

    #[test]
    fn deprecation_warnings_only_for_non_default_values() {
        assert!(MiniChatConfig::default().deprecation_warnings().is_empty());

        let cfg = config(json!({
            "estimation_budgets": { "safety_margin_pct": 20, "minimal_generation_floor": 10 },
            "cleanup_worker": { "enabled": false, "max_attempts": 9 },
            "thread_summary_worker": { "reconcile_interval_secs": 5 }
        }));
        let warnings = cfg.deprecation_warnings();
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        let joined = warnings.join("\n");
        for key in [
            "safety_margin_pct",
            "cleanup_worker.enabled",
            "summary_worker.reconcile",
        ] {
            assert!(joined.contains(key), "{key}: {joined}");
        }
    }
}
