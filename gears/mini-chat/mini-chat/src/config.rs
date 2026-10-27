//! Typed gear configuration (`gears.mini-chat.config`).
//!
//! Defaults and validation follow the Mini Chat parameter source map
//! (DESIGN.md Appendix B). Unknown keys are rejected at the top level and in
//! the strict sections; the worker sections (`orphan_watchdog`,
//! `upload_reaper`, `thread_summary_worker`, `cleanup_worker`) accept and
//! ignore unknown keys.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use toolkit_macros::ExpandVars;


/// Built-in default web-search guard instruction appended to the system
/// prompt when the `web_search` tool is sent.
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Built-in default file-search guard instruction appended to the system
/// prompt when the `file_search` tool is sent.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search to answer questions about the uploaded documents when the answer may be contained in them.";

/// Built-in default knowledge-search guard instruction.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to query the organization knowledge base when the answer may be contained in it.";

/// Built-in default system prompt of the thread-summary request.
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Thread summary preamble prepended to the summary in the context.
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// Default summary model when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Default OAGW auth plugin type (API key) used by the default provider entry.
pub const DEFAULT_AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// Provider adapter kind (`providers.<id>.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// `OpenAI` and Azure `OpenAI` `Responses` API.
    OpenaiResponses,
    /// Chat Completions API.
    OpenaiChatCompletions,
    /// `vLLM` `Responses` API.
    VllmResponses,
    /// Anthropic Messages API.
    AnthropicMessages,
}

/// File / vector-store implementation selector (`providers.<id>.storage_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    /// `OpenAI` Files / Vector Stores API (prefix `/v1`).
    Openai,
    /// Azure `OpenAI` Files / Vector Stores API (prefix `/openai`, requires
    /// `api_version`).
    Azure,
}

/// Provider auth configuration map with `${VAR}` expansion support.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig(pub HashMap<String, serde_json::Value>);

impl AuthConfig {
    /// Convert to a plain string map (non-string values are JSON-encoded).
    #[must_use]
    pub fn as_string_map(&self) -> HashMap<String, String> {
        self.0
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (k.clone(), value)
            })
            .collect()
    }
}

impl toolkit_utils::var_expand::ExpandVars for AuthConfig {
    fn expand_vars(&mut self) -> Result<(), toolkit_utils::var_expand::ExpandVarsError> {
        fn walk(v: &mut serde_json::Value) -> Result<(), toolkit_utils::var_expand::ExpandVarsError> {
            match v {
                serde_json::Value::String(s) => s.expand_vars(),
                serde_json::Value::Array(items) => {
                    for item in items {
                        walk(item)?;
                    }
                    Ok(())
                }
                serde_json::Value::Object(map) => {
                    for val in map.values_mut() {
                        walk(val)?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }
        for val in self.0.values_mut() {
            walk(val)?;
        }
        Ok(())
    }
}

/// Per-tenant provider override (`providers.<id>.tenant_overrides.<tenant>`).
///
/// Every field falls back to the provider entry on its own. An override must
/// set `host` or `upstream_alias`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct TenantOverride {
    /// Override host.
    #[expand_vars]
    pub host: Option<String>,
    /// Override upstream alias (filled with `host` at gear init when unset).
    pub upstream_alias: Option<String>,
    /// Override OAGW auth plugin type.
    pub auth_plugin_type: Option<String>,
    /// Override OAGW auth plugin config.
    #[expand_vars]
    pub auth_config: Option<AuthConfig>,
}

/// One provider entry (`providers.<id>`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderEntry {
    /// Adapter kind.
    pub kind: Option<ProviderKind>,
    /// Provider host (also the default upstream alias).
    #[expand_vars]
    pub host: String,
    /// Provider port (default 443, or 80 when `use_http`).
    pub port: Option<u16>,
    /// Use plain HTTP (effective only when OAGW allows HTTP upstreams).
    pub use_http: bool,
    /// OAGW upstream alias; filled with `host` at gear init when unset.
    pub upstream_alias: Option<String>,
    /// Chat endpoint path; `{model}` is replaced by `provider_model_id`.
    pub api_path: String,
    /// OAGW auth plugin type.
    pub auth_plugin_type: Option<String>,
    /// OAGW auth plugin configuration.
    #[expand_vars]
    pub auth_config: Option<AuthConfig>,
    /// File / vector-store implementation selector.
    pub storage_kind: Option<StorageKind>,
    /// Storage routing label stored on attachments and vector store rows.
    pub storage_backend: Option<String>,
    /// Azure `api-version` for RAG requests (required for
    /// `storage_kind = azure`).
    pub api_version: Option<String>,
    /// Provider used for file / vector-store operations when this provider
    /// has no file API (e.g. Anthropic).
    pub rag_provider: Option<String>,
    /// Per-tenant overrides keyed by tenant id.
    #[expand_vars]
    pub tenant_overrides: HashMap<String, TenantOverride>,
}

impl Default for ProviderEntry {
    fn default() -> Self {
        Self {
            kind: None,
            host: String::new(),
            port: None,
            use_http: false,
            upstream_alias: None,
            api_path: "/v1/responses".to_owned(),
            auth_plugin_type: None,
            auth_config: None,
            storage_kind: None,
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: HashMap::new(),
        }
    }
}

/// S2S client credentials exchanged via `authn_resolver` for OAGW
/// provisioning at gear start.
#[derive(Debug, Clone, Default, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCredentials {
    /// Client id.
    #[expand_vars]
    pub client_id: String,
    /// Client secret.
    #[expand_vars]
    pub client_secret: secrecy::SecretString,
}

/// Streaming (SSE) configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// SSE `ping` interval in seconds (only before the first content event).
    pub sse_ping_interval_seconds: u16,
    /// Bounded channel capacity between provider task and SSE writer.
    pub sse_channel_capacity: u16,
    /// Cap on `max_output_tokens_applied`.
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

/// Gear-level estimation budgets. Only `minimal_generation_floor` is used;
/// the other fields are deprecated (parsed, not validated, warned about at
/// startup when set to a non-default value).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EstimationBudgetsConfig {
    /// Minimum output token charge for estimated settlement.
    pub minimal_generation_floor: u32,
    /// Deprecated; unused.
    pub bytes_per_token_conservative: u32,
    /// Deprecated; unused.
    pub fixed_overhead_tokens: u32,
    /// Deprecated; unused.
    pub safety_margin_pct: u32,
    /// Deprecated; unused.
    pub image_token_budget: u32,
    /// Deprecated; unused.
    pub tool_surcharge_tokens: u32,
    /// Deprecated; unused.
    pub web_search_surcharge_tokens: u32,
    /// Deprecated; unused.
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

/// Quota configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    /// Overshoot tolerance for actual settlement (`actual/reserve` factor).
    pub overshoot_tolerance_factor: f64,
    /// Quota warning threshold percentage (1–99).
    pub warning_threshold_pct: u8,
    /// Hard limit on web search calls started in one turn.
    pub web_search_max_calls_per_message: u32,
    /// Per-user daily web search call limit.
    pub web_search_daily_quota: u32,
    /// Hard limit on code interpreter calls started in one turn.
    pub code_interpreter_max_calls_per_message: u32,
    /// Per-user daily code interpreter call limit.
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

/// Outbox queue names and partitioning.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutboxConfig {
    /// `Usage` events queue.
    pub queue_name: String,
    /// Attachment deletion queue.
    pub cleanup_queue_name: String,
    /// Chat deletion queue.
    pub chat_cleanup_queue_name: String,
    /// Thread summary queue.
    pub thread_summary_queue_name: String,
    /// Audit events queue.
    pub audit_queue_name: String,
    /// Partitions of every mini-chat queue (power of 2, 1–64).
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

/// Context assembly configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Number of recent messages included in the LLM context (0–100).
    pub recent_messages_limit: u32,
    /// Guard instruction appended to the system prompt when `web_search` is
    /// sent.
    pub web_search_guard: String,
    /// Guard instruction appended to the system prompt when `file_search` is
    /// sent.
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

/// RAG / upload limits.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RagConfig {
    /// Maximum document upload size in KiB.
    pub uploaded_file_max_size_kb: u32,
    /// Maximum image upload size in KiB.
    pub uploaded_image_max_size_kb: u32,
    /// Maximum image inputs per message.
    pub max_images_per_message: u32,
    /// Maximum document attachments per chat.
    pub max_documents_per_chat: u32,
    /// Maximum total uploaded size (MiB) per chat, images included.
    pub max_total_upload_mb_per_chat: u32,
    /// Accept `text/csv` uploads (remapped to `text/plain`).
    pub allow_csv_upload: bool,
    /// Concurrent in-flight uploads per process.
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

/// Image thumbnail configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThumbnailConfig {
    /// Target thumbnail width in pixels.
    pub width: u32,
    /// Target thumbnail height in pixels.
    pub height: u32,
    /// Maximum encoded (WebP) thumbnail size in bytes.
    pub max_bytes: usize,
    /// Maximum source image pixel count before skipping thumbnailing.
    pub max_pixels: u64,
    /// Maximum uploaded image size for thumbnailing and maximum decoder
    /// allocation.
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

/// Knowledge search (`search_knowledge`) configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KnowledgeSearchConfig {
    /// Feature toggle (off by default).
    pub enabled: bool,
    /// Knowledge base vector store id (required when enabled).
    pub vector_store_id: Option<String>,
    /// Provider entry used for retrieval (required when enabled).
    pub provider_id: Option<String>,
    /// Maximum `search_knowledge` retrievals per message.
    pub max_calls_per_message: u32,
    /// Top-k cap for retrieval results.
    pub top_k: usize,
    /// Per-chunk character cap.
    pub max_chunk_chars: usize,
    /// Guard instruction appended to the system prompt when
    /// `search_knowledge` is sent.
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

/// Orphan turn watchdog configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct OrphanWatchdogConfig {
    /// Feature toggle.
    pub enabled: bool,
    /// Stale-progress timeout in seconds (90–3600).
    pub timeout_secs: u64,
    /// Scan interval in seconds.
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

/// Upload reaper configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct UploadReaperConfig {
    /// Feature toggle.
    pub enabled: bool,
    /// Scan interval in seconds.
    pub scan_interval_secs: u64,
    /// Age after which a `pending`/`uploaded` row is failed (60–86400).
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

/// Thread summary worker configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ThreadSummaryWorkerConfig {
    /// Feature toggle (gates the trigger in finalization).
    pub enabled: bool,
    /// Outbox lease of the thread-summary queue (30–3600 s).
    pub claim_timeout_secs: u64,
    /// Deliveries before the task is dead-lettered.
    pub max_attempts: u32,
    /// Proactive trigger threshold (percent of the input budget, 1–99).
    pub compression_threshold_pct: u32,
    /// Catalog model used for summaries (empty = `gpt-4.1-mini`).
    pub summary_model_id: String,
    /// Fallback system prompt when the catalog `thread_summary_prompt` is
    /// empty.
    pub summary_system_prompt: String,
    /// Max characters per message in the prompt (0 = no truncation).
    pub message_content_limit: usize,
    /// Deprecated; no effect.
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

/// Cleanup worker configuration. Only `max_attempts` has an effect; the
/// other fields are deprecated (parsed, not validated, warned about at
/// startup when set to a non-default value).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct CleanupWorkerConfig {
    /// Provider delete attempts per attachment before terminal `failed`;
    /// also the delivery limit of a chat cleanup message whose vector-store
    /// delete fails.
    pub max_attempts: u32,
    /// Deprecated; no effect.
    pub enabled: bool,
    /// Deprecated; no effect.
    pub poll_interval_secs: u64,
    /// Deprecated; no effect.
    pub reconcile_interval_secs: u64,
    /// Deprecated; no effect.
    pub stale_in_progress_timeout_secs: u64,
    /// Deprecated; no effect.
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

/// Metrics configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix (empty = `mini_chat`).
    pub prefix: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            prefix: String::new(),
        }
    }
}

/// The mini-chat gear configuration.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    /// Prefix of all REST routes.
    pub url_prefix: String,
    /// Plugin vendor selector; must match the registered plugin instances.
    pub vendor: String,
    /// S2S client credentials for OAGW provisioning.
    #[expand_vars]
    pub client_credentials: Option<ClientCredentials>,
    /// Metrics configuration.
    pub metrics: MetricsConfig,
    /// Provider registry keyed by provider id.
    #[expand_vars]
    pub providers: HashMap<String, ProviderEntry>,
    /// Streaming configuration.
    pub streaming: StreamingConfig,
    /// Gear-level estimation budgets.
    pub estimation_budgets: EstimationBudgetsConfig,
    /// Quota configuration.
    pub quota: QuotaConfig,
    /// Outbox configuration.
    pub outbox: OutboxConfig,
    /// Context assembly configuration.
    pub context: ContextConfig,
    /// RAG / upload limits.
    pub rag: RagConfig,
    /// Thumbnail configuration.
    pub thumbnail: ThumbnailConfig,
    /// Knowledge search configuration.
    pub knowledge_search: KnowledgeSearchConfig,
    /// Orphan watchdog configuration.
    pub orphan_watchdog: OrphanWatchdogConfig,
    /// Upload reaper configuration.
    pub upload_reaper: UploadReaperConfig,
    /// Thread summary worker configuration.
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    /// Cleanup worker configuration.
    pub cleanup_worker: CleanupWorkerConfig,
}

impl Default for MiniChatConfig {
    fn default() -> Self {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_owned(),
            ProviderEntry {
                kind: Some(ProviderKind::OpenaiResponses),
                host: "api.openai.com".to_owned(),
                auth_plugin_type: Some(DEFAULT_AUTH_PLUGIN_TYPE.to_owned()),
                auth_config: Some(AuthConfig(HashMap::from([
                    ("header".to_owned(), serde_json::Value::from("Authorization")),
                    ("prefix".to_owned(), serde_json::Value::from("Bearer ")),
                    ("secret_ref".to_owned(), serde_json::Value::from("cred://openai-key")),
                ]))),
                storage_kind: Some(StorageKind::Openai),
                ..ProviderEntry::default()
            },
        );
        Self {
            url_prefix: "/mini-chat".to_owned(),
            vendor: "constructorfabric".to_owned(),
            client_credentials: None,
            metrics: MetricsConfig::default(),
            providers,
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

/// Errors returned by [`MiniChatConfig::validate`].
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

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

impl MiniChatConfig {
    /// Fill derived defaults that depend on other fields (upstream aliases).
    pub fn normalize(&mut self) {
        for (id, entry) in &mut self.providers {
            if entry.upstream_alias.is_none() {
                entry.upstream_alias = Some(entry.host.clone());
            }
            if entry.storage_backend.is_none() {
                entry.storage_backend = Some(id.clone());
            }
            for override_ in entry.tenant_overrides.values_mut() {
                if override_.upstream_alias.is_none() {
                    override_.upstream_alias = override_.host.clone();
                }
            }
        }
    }

    /// Validate every section. Returns the first violation found.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] describing the invalid key.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let err = |msg: String| ConfigError(msg);
        if self.vendor.is_empty() {
            return Err(err("vendor must be non-empty".to_owned()));
        }
        match &self.client_credentials {
            Some(cc) => {
                use secrecy::ExposeSecret;
                if cc.client_id.is_empty() || cc.client_secret.expose_secret().is_empty() {
                    return Err(err(
                        "client_credentials.client_id and client_secret must be non-empty"
                            .to_owned(),
                    ));
                }
            }
            None => return Err(err("client_credentials is required".to_owned())),
        }
        if !(5..=60).contains(&self.streaming.sse_ping_interval_seconds) {
            return Err(err(
                "streaming.sse_ping_interval_seconds must be in 5..=60".to_owned(),
            ));
        }
        if !(16..=64).contains(&self.streaming.sse_channel_capacity) {
            return Err(err(
                "streaming.sse_channel_capacity must be in 16..=64".to_owned(),
            ));
        }
        if self.estimation_budgets.minimal_generation_floor == 0
            || self.estimation_budgets.minimal_generation_floor > self.streaming.max_output_tokens
        {
            return Err(err(
                "estimation_budgets.minimal_generation_floor must be > 0 and <= streaming.max_output_tokens"
                    .to_owned(),
            ));
        }
        if !(1.0..=1.5).contains(&self.quota.overshoot_tolerance_factor) {
            return Err(err(
                "quota.overshoot_tolerance_factor must be in 1.0..=1.5".to_owned(),
            ));
        }
        if !(1..=99).contains(&self.quota.warning_threshold_pct) {
            return Err(err("quota.warning_threshold_pct must be in 1..=99".to_owned()));
        }
        if self.quota.web_search_max_calls_per_message == 0 {
            return Err(err(
                "quota.web_search_max_calls_per_message must be > 0".to_owned(),
            ));
        }
        if self.quota.web_search_daily_quota == 0 {
            return Err(err("quota.web_search_daily_quota must be > 0".to_owned()));
        }
        if self.quota.code_interpreter_max_calls_per_message == 0 {
            return Err(err(
                "quota.code_interpreter_max_calls_per_message must be > 0".to_owned(),
            ));
        }
        if self.quota.code_interpreter_daily_quota == 0 {
            return Err(err("quota.code_interpreter_daily_quota must be > 0".to_owned()));
        }
        if self.outbox.queue_name.is_empty()
            || self.outbox.cleanup_queue_name.is_empty()
            || self.outbox.chat_cleanup_queue_name.is_empty()
            || self.outbox.thread_summary_queue_name.is_empty()
            || self.outbox.audit_queue_name.is_empty()
        {
            return Err(err("outbox queue names must be non-empty".to_owned()));
        }
        if !(1..=64).contains(&self.outbox.num_partitions)
            || !self.outbox.num_partitions.is_power_of_two()
        {
            return Err(err(
                "outbox.num_partitions must be a power of 2 in 1..=64".to_owned(),
            ));
        }
        if self.context.recent_messages_limit > 100 {
            return Err(err(
                "context.recent_messages_limit must be in 0..=100".to_owned(),
            ));
        }
        if self.rag.uploaded_file_max_size_kb == 0 {
            return Err(err("rag.uploaded_file_max_size_kb must be > 0".to_owned()));
        }
        if self.rag.uploaded_image_max_size_kb == 0 {
            return Err(err("rag.uploaded_image_max_size_kb must be > 0".to_owned()));
        }
        if self.rag.max_images_per_message == 0 {
            return Err(err("rag.max_images_per_message must be > 0".to_owned()));
        }
        if self.rag.max_documents_per_chat == 0 {
            return Err(err("rag.max_documents_per_chat must be > 0".to_owned()));
        }
        if self.rag.max_total_upload_mb_per_chat == 0 {
            return Err(err("rag.max_total_upload_mb_per_chat must be > 0".to_owned()));
        }
        if !(1..=256).contains(&self.rag.max_concurrent_uploads) {
            return Err(err("rag.max_concurrent_uploads must be in 1..=256".to_owned()));
        }
        if self.thumbnail.width == 0
            || self.thumbnail.height == 0
            || self.thumbnail.max_bytes == 0
            || self.thumbnail.max_pixels == 0
            || self.thumbnail.max_decode_bytes == 0
        {
            return Err(err("thumbnail fields must be > 0".to_owned()));
        }
        if self.knowledge_search.enabled {
            if self
                .knowledge_search
                .vector_store_id
                .as_deref()
                .is_none_or(str::is_empty)
            {
                return Err(err(
                    "knowledge_search.vector_store_id is required when enabled".to_owned(),
                ));
            }
            if self
                .knowledge_search
                .provider_id
                .as_deref()
                .is_none_or(str::is_empty)
            {
                return Err(err(
                    "knowledge_search.provider_id is required when enabled".to_owned(),
                ));
            }
        }
        if self.knowledge_search.max_calls_per_message == 0 {
            return Err(err(
                "knowledge_search.max_calls_per_message must be > 0".to_owned(),
            ));
        }
        if self.knowledge_search.top_k == 0 {
            return Err(err("knowledge_search.top_k must be > 0".to_owned()));
        }
        if self.knowledge_search.max_chunk_chars == 0 {
            return Err(err("knowledge_search.max_chunk_chars must be > 0".to_owned()));
        }
        if !(90..=3600).contains(&self.orphan_watchdog.timeout_secs) {
            return Err(err(
                "orphan_watchdog.timeout_secs must be in 90..=3600".to_owned(),
            ));
        }
        if !(1..=3600).contains(&self.orphan_watchdog.scan_interval_secs) {
            return Err(err(
                "orphan_watchdog.scan_interval_secs must be in 1..=3600".to_owned(),
            ));
        }
        if !(1..=3600).contains(&self.upload_reaper.scan_interval_secs) {
            return Err(err(
                "upload_reaper.scan_interval_secs must be in 1..=3600".to_owned(),
            ));
        }
        if !(60..=86400).contains(&self.upload_reaper.stale_after_secs) {
            return Err(err(
                "upload_reaper.stale_after_secs must be in 60..=86400".to_owned(),
            ));
        }
        if !(30..=3600).contains(&self.thread_summary_worker.claim_timeout_secs) {
            return Err(err(
                "thread_summary_worker.claim_timeout_secs must be in 30..=3600".to_owned(),
            ));
        }
        if self.thread_summary_worker.max_attempts == 0 {
            return Err(err("thread_summary_worker.max_attempts must be > 0".to_owned()));
        }
        if !(1..=99).contains(&self.thread_summary_worker.compression_threshold_pct) {
            return Err(err(
                "thread_summary_worker.compression_threshold_pct must be in 1..=99".to_owned(),
            ));
        }
        if self.cleanup_worker.max_attempts == 0 {
            return Err(err("cleanup_worker.max_attempts must be > 0".to_owned()));
        }
        self.validate_providers()?;
        Ok(())
    }

    fn validate_providers(&self) -> Result<(), ConfigError> {
        let err = |msg: String| ConfigError(msg);
        for (id, entry) in &self.providers {
            let Some(kind) = entry.kind else {
                return Err(err(format!("providers.{id}.kind is required")));
            };
            if !is_valid_host(&entry.host) {
                return Err(err(format!(
                    "providers.{id}.host must be non-empty and contain only letters, digits, '.', '-', '_', ':', '[', ']'"
                )));
            }
            if entry.port == Some(0) {
                return Err(err(format!("providers.{id}.port must not be 0")));
            }
            if let Some(api_version) = &entry.api_version
                && !is_valid_api_version(api_version)
            {
                return Err(err(format!(
                    "providers.{id}.api_version must contain only letters, digits, '.', '-'"
                )));
            }
            match entry.storage_kind {
                Some(StorageKind::Azure) => {
                    if entry
                        .api_version
                        .as_deref()
                        .is_none_or(|v| v.trim().is_empty())
                    {
                        return Err(err(format!(
                            "providers.{id}.api_version is required for storage_kind = azure"
                        )));
                    }
                }
                Some(StorageKind::Openai) => {}
                None => {
                    if kind != ProviderKind::AnthropicMessages {
                        return Err(err(format!(
                            "providers.{id}.storage_kind is required for kind {kind:?}"
                        )));
                    }
                }
            }
            if kind == ProviderKind::AnthropicMessages && entry.rag_provider.is_none() {
                return Err(err(format!(
                    "providers.{id}.rag_provider is required for anthropic_messages providers"
                )));
            }
            if let Some(rag) = &entry.rag_provider
                && !self.providers.contains_key(rag)
            {
                return Err(err(format!(
                    "providers.{id}.rag_provider references unknown provider {rag}"
                )));
            }
            for (tenant, override_) in &entry.tenant_overrides {
                if override_.host.is_none() && override_.upstream_alias.is_none() {
                    return Err(err(format!(
                        "providers.{id}.tenant_overrides.{tenant} must set host or upstream_alias"
                    )));
                }
                if let Some(host) = &override_.host
                    && !is_valid_host(host)
                {
                    return Err(err(format!(
                        "providers.{id}.tenant_overrides.{tenant}.host has invalid characters"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Log startup warnings for deprecated fields set to non-default values.
    pub fn warn_deprecated(&self) {
        let defaults = EstimationBudgetsConfig::default();
        let eb = &self.estimation_budgets;
        for (name, set) in [
            (
                "estimation_budgets.bytes_per_token_conservative",
                eb.bytes_per_token_conservative != defaults.bytes_per_token_conservative,
            ),
            (
                "estimation_budgets.fixed_overhead_tokens",
                eb.fixed_overhead_tokens != defaults.fixed_overhead_tokens,
            ),
            (
                "estimation_budgets.safety_margin_pct",
                eb.safety_margin_pct != defaults.safety_margin_pct,
            ),
            (
                "estimation_budgets.image_token_budget",
                eb.image_token_budget != defaults.image_token_budget,
            ),
            (
                "estimation_budgets.tool_surcharge_tokens",
                eb.tool_surcharge_tokens != defaults.tool_surcharge_tokens,
            ),
            (
                "estimation_budgets.web_search_surcharge_tokens",
                eb.web_search_surcharge_tokens != defaults.web_search_surcharge_tokens,
            ),
            (
                "estimation_budgets.code_interpreter_surcharge_tokens",
                eb.code_interpreter_surcharge_tokens != defaults.code_interpreter_surcharge_tokens,
            ),
        ] {
            if set {
                tracing::warn!("mini-chat config: {name} is deprecated and has no effect");
            }
        }
        let cw = &self.cleanup_worker;
        let cw_defaults = CleanupWorkerConfig::default();
        for (name, set) in [
            ("cleanup_worker.enabled", cw.enabled != cw_defaults.enabled),
            (
                "cleanup_worker.poll_interval_secs",
                cw.poll_interval_secs != cw_defaults.poll_interval_secs,
            ),
            (
                "cleanup_worker.reconcile_interval_secs",
                cw.reconcile_interval_secs != cw_defaults.reconcile_interval_secs,
            ),
            (
                "cleanup_worker.stale_in_progress_timeout_secs",
                cw.stale_in_progress_timeout_secs != cw_defaults.stale_in_progress_timeout_secs,
            ),
            (
                "cleanup_worker.batch_size",
                cw.batch_size != cw_defaults.batch_size,
            ),
            (
                "thread_summary_worker.reconcile_interval_secs",
                self.thread_summary_worker.reconcile_interval_secs
                    != ThreadSummaryWorkerConfig::default().reconcile_interval_secs,
            ),
        ] {
            if set {
                tracing::warn!("mini-chat config: {name} is deprecated and has no effect");
            }
        }
    }
}
