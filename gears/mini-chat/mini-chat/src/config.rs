//! Gear configuration (`gears.mini-chat.config`), DESIGN Appendix B.

use std::collections::HashMap;

use serde::Deserialize;
use toolkit_macros::ExpandVars;

/// Default summary model when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL: &str = "gpt-4.1-mini";

/// Built-in web search guard (`context.web_search_guard`).
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Built-in file search guard (`context.file_search_guard`).
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Documents uploaded to this chat are searchable with the file_search tool. Use file_search when the question refers to the uploaded documents, and base such answers on the retrieved excerpts.";

/// Built-in knowledge search guard (`knowledge_search.guard`).
pub const DEFAULT_KNOWLEDGE_GUARD: &str = "Use the search_knowledge tool to look up the organization knowledge base when the question needs it. Answer from the retrieved excerpts.";

/// Built-in summary system prompt (DESIGN B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Top-level gear configuration.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    /// Route prefix.
    pub url_prefix: String,
    /// Plugin vendor.
    pub vendor: String,
    /// S2S credentials.
    #[expand_vars]
    pub client_credentials: ClientCredentialsConfig,
    /// Metrics.
    pub metrics: MetricsConfig,
    /// Provider entries keyed by provider id.
    #[expand_vars]
    pub providers: HashMap<String, ProviderEntry>,
    /// Streaming.
    pub streaming: StreamingConfig,
    /// Estimation budgets (only `minimal_generation_floor` is used).
    pub estimation_budgets: GearEstimationBudgets,
    /// Quota knobs.
    pub quota: QuotaConfig,
    /// Outbox queues.
    pub outbox: OutboxConfig,
    /// Context assembly.
    pub context: ContextConfig,
    /// Uploads / RAG.
    pub rag: RagConfig,
    /// Thumbnails.
    pub thumbnail: ThumbnailConfig,
    /// Knowledge search.
    pub knowledge_search: KnowledgeSearchConfig,
    /// Orphan watchdog.
    pub orphan_watchdog: OrphanWatchdogConfig,
    /// Upload reaper.
    pub upload_reaper: UploadReaperConfig,
    /// Thread summary worker.
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    /// Cleanup worker.
    pub cleanup_worker: CleanupWorkerConfig,
}

impl Default for MiniChatConfig {
    fn default() -> Self {
        let mut providers = HashMap::new();
        providers.insert("openai".to_owned(), ProviderEntry::default_openai());
        Self {
            url_prefix: "/mini-chat".to_owned(),
            vendor: "constructorfabric".to_owned(),
            client_credentials: ClientCredentialsConfig::default(),
            metrics: MetricsConfig::default(),
            providers,
            streaming: StreamingConfig::default(),
            estimation_budgets: GearEstimationBudgets::default(),
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

/// S2S client credentials.
#[derive(Clone, Default, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCredentialsConfig {
    /// Client id.
    #[expand_vars]
    pub client_id: String,
    /// Client secret (redacted in logs).
    #[expand_vars]
    pub client_secret: String,
}

impl std::fmt::Debug for ClientCredentialsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientCredentialsConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .finish()
    }
}

/// Metrics.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix (`""` = `mini_chat`).
    pub prefix: String,
}

impl MetricsConfig {
    /// Effective prefix.
    #[must_use]
    pub fn effective_prefix(&self) -> &str {
        if self.prefix.is_empty() { "mini_chat" } else { &self.prefix }
    }
}

/// Provider adapter kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// `OpenAI` / Azure `OpenAI` Responses API.
    OpenaiResponses,
    /// Chat Completions API.
    OpenaiChatCompletions,
    /// vLLM Responses API.
    VllmResponses,
    /// Anthropic Messages API.
    AnthropicMessages,
}

/// RAG storage implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    /// `OpenAI` (`/v1/...`).
    Openai,
    /// Azure `OpenAI` (`/openai/...?api-version=`).
    Azure,
}

/// One `providers.<id>` entry.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    /// Adapter kind.
    pub kind: ProviderKind,
    /// Host (OAGW endpoint).
    #[expand_vars]
    pub host: String,
    /// Port (default 443, or 80 with `use_http`).
    #[serde(default)]
    pub port: Option<u16>,
    /// Plain HTTP upstream.
    #[serde(default)]
    pub use_http: bool,
    /// OAGW alias (defaults to the host).
    #[serde(default)]
    pub upstream_alias: Option<String>,
    /// Chat endpoint path; `{model}` is replaced by `provider_model_id`.
    #[serde(default = "default_api_path")]
    pub api_path: String,
    /// OAGW auth plugin type.
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    /// OAGW auth plugin config.
    #[serde(default)]
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
    /// Storage implementation (required).
    #[serde(default)]
    pub storage_kind: Option<StorageKind>,
    /// Storage backend label.
    #[serde(default)]
    pub storage_backend: Option<String>,
    /// Azure `api-version` for RAG requests.
    #[serde(default)]
    pub api_version: Option<String>,
    /// Provider used for files and vector stores.
    #[serde(default)]
    pub rag_provider: Option<String>,
    /// Per-tenant overrides.
    #[serde(default)]
    #[expand_vars]
    pub tenant_overrides: HashMap<String, TenantOverride>,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

impl ProviderEntry {
    fn default_openai() -> Self {
        let mut auth = HashMap::new();
        auth.insert("header".to_owned(), "authorization".to_owned());
        auth.insert("prefix".to_owned(), "Bearer ".to_owned());
        auth.insert("secret_ref".to_owned(), "cred://openai-key".to_owned());
        Self {
            kind: ProviderKind::OpenaiResponses,
            host: "api.openai.com".to_owned(),
            port: None,
            use_http: false,
            upstream_alias: None,
            api_path: default_api_path(),
            auth_plugin_type: Some(
                "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned(),
            ),
            auth_config: Some(auth),
            storage_kind: Some(StorageKind::Openai),
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: HashMap::new(),
        }
    }

    /// Effective port.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }
}

/// Per-tenant provider override.
#[derive(Debug, Clone, Default, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct TenantOverride {
    /// Host.
    #[expand_vars]
    pub host: Option<String>,
    /// OAGW alias.
    pub upstream_alias: Option<String>,
    /// Auth plugin type.
    pub auth_plugin_type: Option<String>,
    /// Auth config.
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
}

/// Streaming knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// Ping interval before the first content event.
    pub sse_ping_interval_seconds: u16,
    /// Bounded channel capacity.
    pub sse_channel_capacity: u16,
    /// Cap on `max_output_tokens_applied`.
    pub max_output_tokens: u32,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self { sse_ping_interval_seconds: 15, sse_channel_capacity: 32, max_output_tokens: 32768 }
    }
}

/// Gear `estimation_budgets` section; only `minimal_generation_floor` is used.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct GearEstimationBudgets {
    /// Minimal output tokens charged on estimated settlement.
    pub minimal_generation_floor: u32,
    /// Deprecated.
    pub bytes_per_token_conservative: u32,
    /// Deprecated.
    pub fixed_overhead_tokens: u32,
    /// Deprecated.
    pub safety_margin_pct: u32,
    /// Deprecated.
    pub image_token_budget: u32,
    /// Deprecated.
    pub tool_surcharge_tokens: u32,
    /// Deprecated.
    pub web_search_surcharge_tokens: u32,
    /// Deprecated.
    pub code_interpreter_surcharge_tokens: u32,
}

impl Default for GearEstimationBudgets {
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

/// Quota knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    /// Overshoot tolerance factor.
    pub overshoot_tolerance_factor: f64,
    /// Warning threshold in percent.
    pub warning_threshold_pct: u8,
    /// Per-turn web search limit.
    pub web_search_max_calls_per_message: u32,
    /// Daily web search quota.
    pub web_search_daily_quota: u32,
    /// Per-turn code interpreter limit.
    pub code_interpreter_max_calls_per_message: u32,
    /// Daily code interpreter quota.
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

/// Outbox queues.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutboxConfig {
    /// Usage queue.
    pub queue_name: String,
    /// Attachment cleanup queue.
    pub cleanup_queue_name: String,
    /// Chat cleanup queue.
    pub chat_cleanup_queue_name: String,
    /// Thread summary queue.
    pub thread_summary_queue_name: String,
    /// Audit queue.
    pub audit_queue_name: String,
    /// Partitions per queue.
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

/// Context assembly.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Recent messages included.
    pub recent_messages_limit: u32,
    /// Web search guard.
    pub web_search_guard: String,
    /// File search guard.
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

/// Upload / RAG limits.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RagConfig {
    /// Document size limit in KiB.
    pub uploaded_file_max_size_kb: u32,
    /// Image size limit in KiB.
    pub uploaded_image_max_size_kb: u32,
    /// Images per message.
    pub max_images_per_message: u32,
    /// Documents per chat.
    pub max_documents_per_chat: u32,
    /// Total upload size per chat in MiB.
    pub max_total_upload_mb_per_chat: u32,
    /// Accept CSV as text/plain.
    pub allow_csv_upload: bool,
    /// Concurrent uploads per process.
    pub max_concurrent_uploads: u16,
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            uploaded_file_max_size_kb: 25_600,
            uploaded_image_max_size_kb: 5_120,
            max_images_per_message: 4,
            max_documents_per_chat: 50,
            max_total_upload_mb_per_chat: 100,
            allow_csv_upload: true,
            max_concurrent_uploads: 10,
        }
    }
}

/// Thumbnails.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThumbnailConfig {
    /// Target width.
    pub width: u32,
    /// Target height.
    pub height: u32,
    /// Max encoded size.
    pub max_bytes: usize,
    /// Max source pixels.
    pub max_pixels: u64,
    /// Max decode bytes.
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

/// Knowledge search.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KnowledgeSearchConfig {
    /// Enabled.
    pub enabled: bool,
    /// Knowledge vector store.
    pub vector_store_id: Option<String>,
    /// Provider serving the knowledge store.
    pub provider_id: Option<String>,
    /// Retrievals per message.
    pub max_calls_per_message: u32,
    /// Top-k cap.
    pub top_k: usize,
    /// Max characters per chunk.
    pub max_chunk_chars: usize,
    /// Guard text.
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
            guard: DEFAULT_KNOWLEDGE_GUARD.to_owned(),
        }
    }
}

/// Orphan watchdog (accepts unknown keys).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrphanWatchdogConfig {
    /// Enabled.
    pub enabled: bool,
    /// Stale progress timeout.
    pub timeout_secs: u64,
    /// Scan interval.
    pub scan_interval_secs: u64,
}

impl Default for OrphanWatchdogConfig {
    fn default() -> Self {
        Self { enabled: true, timeout_secs: 300, scan_interval_secs: 60 }
    }
}

/// Upload reaper (accepts unknown keys).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UploadReaperConfig {
    /// Enabled.
    pub enabled: bool,
    /// Scan interval.
    pub scan_interval_secs: u64,
    /// Staleness threshold.
    pub stale_after_secs: u64,
}

impl Default for UploadReaperConfig {
    fn default() -> Self {
        Self { enabled: true, scan_interval_secs: 60, stale_after_secs: 300 }
    }
}

/// Thread summary worker (accepts unknown keys).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThreadSummaryWorkerConfig {
    /// Trigger enabled.
    pub enabled: bool,
    /// Outbox lease.
    pub claim_timeout_secs: u64,
    /// Deliveries before dead-letter.
    pub max_attempts: u32,
    /// Proactive trigger threshold.
    pub compression_threshold_pct: u32,
    /// Summary model (`""` = gpt-4.1-mini).
    pub summary_model_id: String,
    /// Fallback system prompt.
    pub summary_system_prompt: String,
    /// Max characters per message in the prompt.
    pub message_content_limit: usize,
    /// Deprecated.
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
    /// Effective summary model id.
    #[must_use]
    pub fn effective_model_id(&self) -> &str {
        if self.summary_model_id.trim().is_empty() {
            DEFAULT_SUMMARY_MODEL
        } else {
            self.summary_model_id.trim()
        }
    }
}

/// Cleanup worker (accepts unknown keys).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CleanupWorkerConfig {
    /// Max provider delete attempts.
    pub max_attempts: u32,
    /// Deprecated.
    pub enabled: bool,
    /// Deprecated.
    pub poll_interval_secs: u64,
    /// Deprecated.
    pub reconcile_interval_secs: u64,
    /// Deprecated.
    pub stale_in_progress_timeout_secs: u64,
    /// Deprecated.
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

fn host_chars_ok(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

impl MiniChatConfig {
    /// Validates every section; returns a descriptive error.
    ///
    /// # Errors
    /// The first violated rule.
    #[allow(clippy::too_many_lines, reason = "flat list of independent config validation rules")]
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.trim().is_empty() {
            return Err("vendor must not be empty".into());
        }
        if self.client_credentials.client_id.trim().is_empty()
            || self.client_credentials.client_secret.trim().is_empty()
        {
            return Err("client_credentials.client_id and client_secret are required".into());
        }
        let stream = &self.streaming;
        if !(5..=60).contains(&stream.sse_ping_interval_seconds) {
            return Err("streaming.sse_ping_interval_seconds must be in 5..=60".into());
        }
        if !(16..=64).contains(&stream.sse_channel_capacity) {
            return Err("streaming.sse_channel_capacity must be in 16..=64".into());
        }
        if stream.max_output_tokens == 0 {
            return Err("streaming.max_output_tokens must be > 0".into());
        }
        let floor = self.estimation_budgets.minimal_generation_floor;
        if floor == 0 || floor > stream.max_output_tokens {
            return Err(
                "estimation_budgets.minimal_generation_floor must be > 0 and <= streaming.max_output_tokens"
                    .into(),
            );
        }
        let quota = &self.quota;
        if !(1.0..=1.5).contains(&quota.overshoot_tolerance_factor) {
            return Err("quota.overshoot_tolerance_factor must be in 1.0..=1.5".into());
        }
        if !(1..=99).contains(&quota.warning_threshold_pct) {
            return Err("quota.warning_threshold_pct must be in 1..=99".into());
        }
        for (name, v) in [
            ("web_search_max_calls_per_message", quota.web_search_max_calls_per_message),
            ("web_search_daily_quota", quota.web_search_daily_quota),
            ("code_interpreter_max_calls_per_message", quota.code_interpreter_max_calls_per_message),
            ("code_interpreter_daily_quota", quota.code_interpreter_daily_quota),
        ] {
            if v == 0 {
                return Err(format!("quota.{name} must be > 0"));
            }
        }
        let outbox = &self.outbox;
        for (name, v) in [
            ("queue_name", &outbox.queue_name),
            ("cleanup_queue_name", &outbox.cleanup_queue_name),
            ("chat_cleanup_queue_name", &outbox.chat_cleanup_queue_name),
            ("thread_summary_queue_name", &outbox.thread_summary_queue_name),
            ("audit_queue_name", &outbox.audit_queue_name),
        ] {
            if v.trim().is_empty() {
                return Err(format!("outbox.{name} must not be empty"));
            }
        }
        if !(1..=64).contains(&outbox.num_partitions) || !outbox.num_partitions.is_power_of_two() {
            return Err("outbox.num_partitions must be a power of two in 1..=64".into());
        }
        if self.context.recent_messages_limit > 100 {
            return Err("context.recent_messages_limit must be in 0..=100".into());
        }
        let rag = &self.rag;
        for (name, v) in [
            ("uploaded_file_max_size_kb", rag.uploaded_file_max_size_kb),
            ("uploaded_image_max_size_kb", rag.uploaded_image_max_size_kb),
            ("max_images_per_message", rag.max_images_per_message),
            ("max_documents_per_chat", rag.max_documents_per_chat),
            ("max_total_upload_mb_per_chat", rag.max_total_upload_mb_per_chat),
        ] {
            if v == 0 {
                return Err(format!("rag.{name} must be > 0"));
            }
        }
        if !(1..=256).contains(&rag.max_concurrent_uploads) {
            return Err("rag.max_concurrent_uploads must be in 1..=256".into());
        }
        let thumb = &self.thumbnail;
        if thumb.width == 0 || thumb.height == 0 || thumb.max_bytes == 0 || thumb.max_pixels == 0 || thumb.max_decode_bytes == 0 {
            return Err("thumbnail values must be > 0".into());
        }
        let search = &self.knowledge_search;
        if search.max_calls_per_message == 0 || search.top_k == 0 || search.max_chunk_chars == 0 {
            return Err("knowledge_search limits must be > 0".into());
        }
        if search.enabled
            && (search.vector_store_id.as_deref().is_none_or(str::is_empty)
                || search.provider_id.as_deref().is_none_or(str::is_empty))
        {
            return Err("knowledge_search.vector_store_id and provider_id are required when enabled".into());
        }
        let watchdog = &self.orphan_watchdog;
        if !(90..=3600).contains(&watchdog.timeout_secs) {
            return Err("orphan_watchdog.timeout_secs must be in 90..=3600".into());
        }
        if !(1..=3600).contains(&watchdog.scan_interval_secs) {
            return Err("orphan_watchdog.scan_interval_secs must be in 1..=3600".into());
        }
        let reaper = &self.upload_reaper;
        if !(1..=3600).contains(&reaper.scan_interval_secs) {
            return Err("upload_reaper.scan_interval_secs must be in 1..=3600".into());
        }
        if !(60..=86_400).contains(&reaper.stale_after_secs) {
            return Err("upload_reaper.stale_after_secs must be in 60..=86400".into());
        }
        let ts = &self.thread_summary_worker;
        if !(30..=3600).contains(&ts.claim_timeout_secs) {
            return Err("thread_summary_worker.claim_timeout_secs must be in 30..=3600".into());
        }
        if ts.max_attempts == 0 {
            return Err("thread_summary_worker.max_attempts must be > 0".into());
        }
        if !(1..=99).contains(&ts.compression_threshold_pct) {
            return Err("thread_summary_worker.compression_threshold_pct must be in 1..=99".into());
        }
        if self.cleanup_worker.max_attempts == 0 {
            return Err("cleanup_worker.max_attempts must be > 0".into());
        }
        self.validate_providers()
    }

    fn validate_providers(&self) -> Result<(), String> {
        if self.providers.is_empty() {
            return Err("providers must contain at least one entry".into());
        }
        for (id, p) in &self.providers {
            if !host_chars_ok(&p.host) {
                return Err(format!("providers.{id}.host is empty or has invalid characters"));
            }
            if p.port == Some(0) {
                return Err(format!("providers.{id}.port must not be 0"));
            }
            if !p.api_path.starts_with('/') {
                return Err(format!("providers.{id}.api_path must start with '/'"));
            }
            match p.storage_kind {
                None => return Err(format!("providers.{id}.storage_kind is required")),
                Some(StorageKind::Azure) => {
                    let v = p.api_version.as_deref().unwrap_or("").trim();
                    if v.is_empty() {
                        return Err(format!(
                            "providers.{id}.api_version is required for storage_kind azure"
                        ));
                    }
                }
                Some(StorageKind::Openai) => {}
            }
            if let Some(v) = &p.api_version
                && !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                return Err(format!("providers.{id}.api_version has invalid characters"));
            }
            if let Some(rag) = &p.rag_provider
                && !self.providers.contains_key(rag)
            {
                return Err(format!("providers.{id}.rag_provider '{rag}' is not a provider entry"));
            }
            for (tid, o) in &p.tenant_overrides {
                if o.host.is_none() && o.upstream_alias.is_none() {
                    return Err(format!(
                        "providers.{id}.tenant_overrides.{tid} must set host or upstream_alias"
                    ));
                }
                if let Some(h) = &o.host
                    && !host_chars_ok(h)
                {
                    return Err(format!("providers.{id}.tenant_overrides.{tid}.host is invalid"));
                }
            }
        }
        if self.knowledge_search.enabled
            && let Some(pid) = &self.knowledge_search.provider_id
            && !self.providers.contains_key(pid)
        {
            return Err(format!("knowledge_search.provider_id '{pid}' is not a provider entry"));
        }
        Ok(())
    }

    /// Names of deprecated fields set to non-default values (logged at startup).
    #[must_use]
    pub fn deprecated_fields_in_use(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let d = GearEstimationBudgets::default();
        let e = &self.estimation_budgets;
        if e.bytes_per_token_conservative != d.bytes_per_token_conservative {
            out.push("estimation_budgets.bytes_per_token_conservative");
        }
        if e.fixed_overhead_tokens != d.fixed_overhead_tokens {
            out.push("estimation_budgets.fixed_overhead_tokens");
        }
        if e.safety_margin_pct != d.safety_margin_pct {
            out.push("estimation_budgets.safety_margin_pct");
        }
        if e.image_token_budget != d.image_token_budget {
            out.push("estimation_budgets.image_token_budget");
        }
        if e.tool_surcharge_tokens != d.tool_surcharge_tokens {
            out.push("estimation_budgets.tool_surcharge_tokens");
        }
        if e.web_search_surcharge_tokens != d.web_search_surcharge_tokens {
            out.push("estimation_budgets.web_search_surcharge_tokens");
        }
        if e.code_interpreter_surcharge_tokens != d.code_interpreter_surcharge_tokens {
            out.push("estimation_budgets.code_interpreter_surcharge_tokens");
        }
        let c = CleanupWorkerConfig::default();
        let w = &self.cleanup_worker;
        if !w.enabled {
            out.push("cleanup_worker.enabled");
        }
        if w.poll_interval_secs != c.poll_interval_secs {
            out.push("cleanup_worker.poll_interval_secs");
        }
        if w.reconcile_interval_secs != c.reconcile_interval_secs {
            out.push("cleanup_worker.reconcile_interval_secs");
        }
        if w.stale_in_progress_timeout_secs != c.stale_in_progress_timeout_secs {
            out.push("cleanup_worker.stale_in_progress_timeout_secs");
        }
        if w.batch_size != c.batch_size {
            out.push("cleanup_worker.batch_size");
        }
        if self.thread_summary_worker.reconcile_interval_secs != 60 {
            out.push("thread_summary_worker.reconcile_interval_secs");
        }
        out
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
