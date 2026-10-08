//! Gear configuration (DESIGN Appendix B).
//!
//! Unknown keys are rejected at the top level and in every section except the
//! worker sections (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`,
//! `cleanup_worker`), which accept and ignore unknown keys.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Default summary model when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Built-in default system prompt of the thread-summary request.
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Default web search guard appended to the system prompt.
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Default file search guard appended to the system prompt.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search to look up information in the documents the user uploaded to this chat when the question may be answered from them. Base your answer on the retrieved excerpts and do not invent document content.";

/// Default knowledge search guard appended to the system prompt.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to look up information in the organization knowledge base when the question may be answered from it. Base your answer on the retrieved excerpts.";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MiniChatConfig {
    pub url_prefix: String,
    pub vendor: String,
    pub client_credentials: Option<ClientCredentialsConfig>,
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
            client_credentials: None,
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

fn default_providers() -> BTreeMap<String, ProviderEntry> {
    let mut auth_config = BTreeMap::new();
    auth_config.insert("header".to_owned(), "Authorization".to_owned());
    auth_config.insert("prefix".to_owned(), "Bearer ".to_owned());
    auth_config.insert("secret_ref".to_owned(), "cred://openai-key".to_owned());
    let mut map = BTreeMap::new();
    map.insert(
        "openai".to_owned(),
        ProviderEntry {
            kind: ProviderKind::OpenaiResponses,
            host: "api.openai.com".to_owned(),
            port: None,
            use_http: false,
            upstream_alias: None,
            api_path: default_api_path(),
            auth_plugin_type: Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned()),
            auth_config,
            storage_kind: Some(StorageKind::Openai),
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: BTreeMap::new(),
        },
    );
    map
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCredentialsConfig {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    pub prefix: String,
}

/// Provider adapter kind (ADR-0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

impl ProviderKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiResponses => "openai_responses",
            Self::OpenaiChatCompletions => "openai_chat_completions",
            Self::VllmResponses => "vllm_responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

/// File / vector-store implementation selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub use_http: bool,
    #[serde(default)]
    pub upstream_alias: Option<String>,
    #[serde(default = "default_api_path")]
    pub api_path: String,
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    #[serde(default)]
    pub auth_config: BTreeMap<String, String>,
    #[serde(default)]
    pub storage_kind: Option<StorageKind>,
    #[serde(default)]
    pub storage_backend: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub rag_provider: Option<String>,
    #[serde(default)]
    pub tenant_overrides: BTreeMap<String, TenantOverride>,
}

impl ProviderEntry {
    /// Effective port (`443`, or `80` with `use_http`).
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThreadSummaryWorkerConfig {
    pub enabled: bool,
    pub claim_timeout_secs: u64,
    pub max_attempts: u32,
    pub compression_threshold_pct: u32,
    pub summary_model_id: String,
    pub summary_system_prompt: String,
    pub message_content_limit: usize,
    /// Deprecated, no effect (ADR-0010).
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
    /// Summary model id (`gpt-4.1-mini` when empty).
    #[must_use]
    pub fn effective_summary_model_id(&self) -> &str {
        if self.summary_model_id.trim().is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            self.summary_model_id.trim()
        }
    }
}

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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

/// Configuration validation error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid mini-chat configuration: {0}")]
pub struct ConfigError(pub String);

fn err(msg: impl Into<String>) -> ConfigError {
    ConfigError(msg.into())
}

/// Expand `${VAR}` references from the process environment. An undefined
/// variable expands to an empty string.
#[must_use]
pub fn expand_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let name = &after[..end];
            out.push_str(&std::env::var(name).unwrap_or_default());
            rest = &after[end + 1..];
        } else {
            out.push_str(&rest[start..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

fn valid_api_version(v: &str) -> bool {
    !v.trim().is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

impl MiniChatConfig {
    /// Expand `${VAR}` references in provider hosts / auth configs, tenant
    /// overrides and client credentials, and fill defaulted fields
    /// (`upstream_alias = host`, override alias = override host).
    pub fn expand_and_normalize(&mut self) {
        if let Some(cc) = self.client_credentials.as_mut() {
            cc.client_id = expand_env(&cc.client_id);
            cc.client_secret = expand_env(&cc.client_secret);
        }
        for entry in self.providers.values_mut() {
            entry.host = expand_env(entry.host.trim());
            for v in entry.auth_config.values_mut() {
                *v = expand_env(v);
            }
            if entry.upstream_alias.as_deref().is_none_or(|a| a.trim().is_empty()) {
                entry.upstream_alias = Some(entry.host.clone());
            }
            for ov in entry.tenant_overrides.values_mut() {
                if let Some(h) = ov.host.as_mut() {
                    *h = expand_env(h.trim());
                }
                if let Some(ac) = ov.auth_config.as_mut() {
                    for v in ac.values_mut() {
                        *v = expand_env(v);
                    }
                }
                if ov.upstream_alias.as_deref().is_none_or(|a| a.trim().is_empty()) {
                    ov.upstream_alias.clone_from(&ov.host);
                }
            }
        }
    }

    /// Log a warning for every deprecated field set to a non-default value.
    #[allow(clippy::cognitive_complexity, reason = "flat list of deprecated-field checks")]
    pub fn warn_deprecated(&self) {
        let eb_default = EstimationBudgetsConfig::default();
        let eb = &self.estimation_budgets;
        let checks: [(&str, bool); 7] = [
            ("bytes_per_token_conservative", eb.bytes_per_token_conservative != eb_default.bytes_per_token_conservative),
            ("fixed_overhead_tokens", eb.fixed_overhead_tokens != eb_default.fixed_overhead_tokens),
            ("safety_margin_pct", eb.safety_margin_pct != eb_default.safety_margin_pct),
            ("image_token_budget", eb.image_token_budget != eb_default.image_token_budget),
            ("tool_surcharge_tokens", eb.tool_surcharge_tokens != eb_default.tool_surcharge_tokens),
            ("web_search_surcharge_tokens", eb.web_search_surcharge_tokens != eb_default.web_search_surcharge_tokens),
            ("code_interpreter_surcharge_tokens", eb.code_interpreter_surcharge_tokens != eb_default.code_interpreter_surcharge_tokens),
        ];
        for (name, changed) in checks {
            if changed {
                tracing::warn!(field = %format!("estimation_budgets.{name}"), "deprecated configuration field has no effect (per-model catalog estimation_budgets are used)");
            }
        }
        let cw = CleanupWorkerConfig::default();
        let c = &self.cleanup_worker;
        let checks: [(&str, bool); 5] = [
            ("enabled", c.enabled != cw.enabled),
            ("poll_interval_secs", c.poll_interval_secs != cw.poll_interval_secs),
            ("reconcile_interval_secs", c.reconcile_interval_secs != cw.reconcile_interval_secs),
            ("stale_in_progress_timeout_secs", c.stale_in_progress_timeout_secs != cw.stale_in_progress_timeout_secs),
            ("batch_size", c.batch_size != cw.batch_size),
        ];
        for (name, changed) in checks {
            if changed {
                tracing::warn!(field = %format!("cleanup_worker.{name}"), "deprecated configuration field has no effect");
            }
        }
        if self.thread_summary_worker.reconcile_interval_secs != 60 {
            tracing::warn!(field = "thread_summary_worker.reconcile_interval_secs", "deprecated configuration field has no effect");
        }
    }

    /// Validate every section. Must be called after [`Self::expand_and_normalize`].
    ///
    /// # Errors
    /// Returns a descriptive [`ConfigError`] for the first invalid value.
    #[allow(
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::many_single_char_names,
        reason = "flat list of per-section validation checks with short section aliases"
    )]
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.vendor.trim().is_empty() {
            return Err(err("vendor must be non-empty"));
        }
        if !self.url_prefix.starts_with('/') {
            return Err(err("url_prefix must start with '/'"));
        }
        match &self.client_credentials {
            None => return Err(err("client_credentials is required")),
            Some(cc) => {
                if cc.client_id.trim().is_empty() || cc.client_secret.trim().is_empty() {
                    return Err(err("client_credentials.client_id and client_secret must be non-empty"));
                }
            }
        }
        // streaming
        let s = &self.streaming;
        if !(5..=60).contains(&s.sse_ping_interval_seconds) {
            return Err(err("streaming.sse_ping_interval_seconds must be in 5..=60"));
        }
        if !(16..=64).contains(&s.sse_channel_capacity) {
            return Err(err("streaming.sse_channel_capacity must be in 16..=64"));
        }
        if s.max_output_tokens == 0 {
            return Err(err("streaming.max_output_tokens must be > 0"));
        }
        // estimation budgets
        let floor = self.estimation_budgets.minimal_generation_floor;
        if floor == 0 || floor > s.max_output_tokens {
            return Err(err(
                "estimation_budgets.minimal_generation_floor must be > 0 and <= streaming.max_output_tokens",
            ));
        }
        // quota
        let q = &self.quota;
        if !(1.0..=1.5).contains(&q.overshoot_tolerance_factor) {
            return Err(err("quota.overshoot_tolerance_factor must be in 1.0..=1.5"));
        }
        if !(1..=99).contains(&q.warning_threshold_pct) {
            return Err(err("quota.warning_threshold_pct must be in 1..=99"));
        }
        if q.web_search_max_calls_per_message == 0
            || q.web_search_daily_quota == 0
            || q.code_interpreter_max_calls_per_message == 0
            || q.code_interpreter_daily_quota == 0
        {
            return Err(err("quota call limits must be > 0"));
        }
        // outbox
        let o = &self.outbox;
        for (name, v) in [
            ("queue_name", &o.queue_name),
            ("cleanup_queue_name", &o.cleanup_queue_name),
            ("chat_cleanup_queue_name", &o.chat_cleanup_queue_name),
            ("thread_summary_queue_name", &o.thread_summary_queue_name),
            ("audit_queue_name", &o.audit_queue_name),
        ] {
            if v.trim().is_empty() {
                return Err(err(format!("outbox.{name} must be non-empty")));
            }
        }
        if !(1..=64).contains(&o.num_partitions) || !o.num_partitions.is_power_of_two() {
            return Err(err("outbox.num_partitions must be a power of two in 1..=64"));
        }
        // context
        if self.context.recent_messages_limit > 100 {
            return Err(err("context.recent_messages_limit must be in 0..=100"));
        }
        // rag
        let r = &self.rag;
        if r.max_documents_per_chat == 0
            || r.max_total_upload_mb_per_chat == 0
            || r.uploaded_file_max_size_kb == 0
            || r.uploaded_image_max_size_kb == 0
            || r.max_images_per_message == 0
        {
            return Err(err("rag limits must be > 0"));
        }
        if !(1..=256).contains(&r.max_concurrent_uploads) {
            return Err(err("rag.max_concurrent_uploads must be in 1..=256"));
        }
        // thumbnail
        let t = &self.thumbnail;
        if t.width == 0 || t.height == 0 || t.max_bytes == 0 || t.max_pixels == 0 || t.max_decode_bytes == 0 {
            return Err(err("thumbnail values must be > 0"));
        }
        // workers
        let w = &self.orphan_watchdog;
        if !(90..=3600).contains(&w.timeout_secs) {
            return Err(err("orphan_watchdog.timeout_secs must be in 90..=3600"));
        }
        if !(1..=3600).contains(&w.scan_interval_secs) {
            return Err(err("orphan_watchdog.scan_interval_secs must be in 1..=3600"));
        }
        let u = &self.upload_reaper;
        if !(1..=3600).contains(&u.scan_interval_secs) {
            return Err(err("upload_reaper.scan_interval_secs must be in 1..=3600"));
        }
        if !(60..=86_400).contains(&u.stale_after_secs) {
            return Err(err("upload_reaper.stale_after_secs must be in 60..=86400"));
        }
        let ts = &self.thread_summary_worker;
        if !(30..=3600).contains(&ts.claim_timeout_secs) {
            return Err(err("thread_summary_worker.claim_timeout_secs must be in 30..=3600"));
        }
        if ts.max_attempts == 0 {
            return Err(err("thread_summary_worker.max_attempts must be > 0"));
        }
        if !(1..=99).contains(&ts.compression_threshold_pct) {
            return Err(err("thread_summary_worker.compression_threshold_pct must be in 1..=99"));
        }
        if self.cleanup_worker.max_attempts == 0 {
            return Err(err("cleanup_worker.max_attempts must be > 0"));
        }
        // knowledge search
        let k = &self.knowledge_search;
        if k.enabled {
            if k.vector_store_id.as_deref().is_none_or(|v| v.trim().is_empty())
                || k.provider_id.as_deref().is_none_or(|v| v.trim().is_empty())
            {
                return Err(err(
                    "knowledge_search.vector_store_id and provider_id are required when enabled",
                ));
            }
            if k.max_calls_per_message == 0 || k.top_k == 0 || k.max_chunk_chars == 0 {
                return Err(err("knowledge_search limits must be > 0"));
            }
        }
        // providers
        if self.providers.is_empty() {
            return Err(err("providers must contain at least one entry"));
        }
        for (id, p) in &self.providers {
            if !valid_host(&p.host) {
                return Err(err(format!(
                    "providers.{id}.host must be non-empty and contain only letters, digits, '.', '-', '_', ':', '[', ']'"
                )));
            }
            if p.port == Some(0) {
                return Err(err(format!("providers.{id}.port must not be 0")));
            }
            if !p.api_path.starts_with('/') {
                return Err(err(format!("providers.{id}.api_path must start with '/'")));
            }
            match p.storage_kind {
                None => return Err(err(format!("providers.{id}.storage_kind is required"))),
                Some(StorageKind::Azure) => {
                    if !p.api_version.as_deref().is_some_and(valid_api_version) {
                        return Err(err(format!(
                            "providers.{id}.api_version is required for storage_kind azure (letters, digits, '.', '-')"
                        )));
                    }
                }
                Some(StorageKind::Openai) => {
                    if let Some(v) = p.api_version.as_deref()
                        && !valid_api_version(v)
                    {
                        return Err(err(format!("providers.{id}.api_version is invalid")));
                    }
                }
            }
            if let Some(rag) = &p.rag_provider
                && !self.providers.contains_key(rag)
            {
                return Err(err(format!(
                    "providers.{id}.rag_provider `{rag}` does not name a provider entry"
                )));
            }
            for (tenant, ov) in &p.tenant_overrides {
                if ov.host.is_none() && ov.upstream_alias.is_none() {
                    return Err(err(format!(
                        "providers.{id}.tenant_overrides.{tenant} must set host or upstream_alias"
                    )));
                }
                if let Some(h) = &ov.host
                    && !valid_host(h)
                {
                    return Err(err(format!(
                        "providers.{id}.tenant_overrides.{tenant}.host is invalid"
                    )));
                }
            }
        }
        if let Some(ks_provider) = &k.provider_id
            && k.enabled
            && !self.providers.contains_key(ks_provider)
        {
            return Err(err(format!(
                "knowledge_search.provider_id `{ks_provider}` does not name a provider entry"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
