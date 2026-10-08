//! Gear configuration (`gears.mini-chat.config`).
//!
//! Unknown keys are rejected at the top level and in the `streaming`,
//! `estimation_budgets`, `quota`, `outbox`, `context`, `rag`,
//! `client_credentials`, `metrics`, `providers.<id>` (and its
//! `tenant_overrides`), `thumbnail` and `knowledge_search` sections. The
//! worker sections (`orphan_watchdog`, `upload_reaper`,
//! `thread_summary_worker`, `cleanup_worker`) accept and ignore unknown keys.
//! Every section is validated by [`MiniChatConfig::validate`].

use std::collections::HashMap;

use serde::Deserialize;
use toolkit::var_expand::{ExpandVars, ExpandVarsError};

/// Built-in default of `thread_summary_worker.summary_system_prompt`.
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Built-in default of `context.web_search_guard`.
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Built-in default of `context.file_search_guard`.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Documents uploaded to this chat are searchable with the file_search tool. Use file_search when the question refers to the uploaded documents or can be answered from them, and ground your answer in the retrieved excerpts.";

/// Built-in default of `knowledge_search.guard`.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "An organization knowledge base is available through the search_knowledge tool. Use it when the question concerns organization-specific information, and base your answer on the returned excerpts.";

/// Default summary model when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    pub url_prefix: String,
    pub vendor: String,
    pub client_credentials: ClientCredentialsConfig,
    pub metrics: MetricsConfig,
    pub streaming: StreamingConfig,
    pub estimation_budgets: EstimationBudgetsConfig,
    pub quota: QuotaConfig,
    pub outbox: OutboxConfig,
    pub context: ContextConfig,
    pub rag: RagConfig,
    pub thumbnail: ThumbnailConfig,
    pub providers: HashMap<String, ProviderEntry>,
    pub orphan_watchdog: OrphanWatchdogConfig,
    pub upload_reaper: UploadReaperConfig,
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    pub cleanup_worker: CleanupWorkerConfig,
    pub knowledge_search: KnowledgeSearchConfig,
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
            streaming: StreamingConfig::default(),
            estimation_budgets: EstimationBudgetsConfig::default(),
            quota: QuotaConfig::default(),
            outbox: OutboxConfig::default(),
            context: ContextConfig::default(),
            rag: RagConfig::default(),
            thumbnail: ThumbnailConfig::default(),
            providers,
            orphan_watchdog: OrphanWatchdogConfig::default(),
            upload_reaper: UploadReaperConfig::default(),
            thread_summary_worker: ThreadSummaryWorkerConfig::default(),
            cleanup_worker: CleanupWorkerConfig::default(),
            knowledge_search: KnowledgeSearchConfig::default(),
        }
    }
}

impl ExpandVars for MiniChatConfig {
    fn expand_vars(&mut self) -> Result<(), ExpandVarsError> {
        self.client_credentials.client_id.expand_vars()?;
        self.client_credentials.client_secret.expand_vars()?;
        for entry in self.providers.values_mut() {
            entry.host.expand_vars()?;
            if let Some(cfg) = entry.auth_config.as_mut() {
                cfg.expand_vars()?;
            }
            for ov in entry.tenant_overrides.values_mut() {
                ov.host.expand_vars()?;
                if let Some(cfg) = ov.auth_config.as_mut() {
                    cfg.expand_vars()?;
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCredentialsConfig {
    pub client_id: String,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub prefix: String,
}

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
            max_output_tokens: 32_768,
        }
    }
}

/// Only `minimal_generation_floor` is used; the other fields are deprecated
/// (the per-model catalog budgets apply) and only produce a startup warning
/// when set to a non-default value.
#[derive(Debug, Clone, Deserialize)]
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

/// Provider adapter kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
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

/// File / vector-store API flavour of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
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
    pub auth_config: Option<HashMap<String, String>>,
    #[serde(default)]
    pub storage_kind: Option<StorageKind>,
    #[serde(default)]
    pub storage_backend: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub rag_provider: Option<String>,
    #[serde(default)]
    pub tenant_overrides: HashMap<String, TenantOverride>,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

impl ProviderEntry {
    fn default_openai() -> Self {
        let mut auth = HashMap::new();
        auth.insert("header".to_owned(), "Authorization".to_owned());
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

    /// Effective port: configured, else 80 for plain HTTP, else 443.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TenantOverride {
    pub host: Option<String>,
    pub upstream_alias: Option<String>,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<HashMap<String, String>>,
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
    /// Effective summary model id (empty → `gpt-4.1-mini`).
    #[must_use]
    pub fn summary_model(&self) -> &str {
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

fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

fn ensure(cond: bool, msg: impl Into<String>) -> Result<(), String> {
    if cond { Ok(()) } else { Err(msg.into()) }
}

impl MiniChatConfig {
    /// Validate every section. Returns a descriptive error message.
    ///
    /// # Errors
    /// Returns the first validation failure.
    #[allow(
        clippy::many_single_char_names,
        clippy::cognitive_complexity,
        clippy::too_many_lines
    )]
    pub fn validate(&self) -> Result<(), String> {
        ensure(!self.vendor.trim().is_empty(), "vendor must be non-empty")?;
        ensure(
            self.url_prefix.is_empty() || self.url_prefix.starts_with('/'),
            "url_prefix must start with '/'",
        )?;
        ensure(
            !self.client_credentials.client_id.trim().is_empty(),
            "client_credentials.client_id must be non-empty",
        )?;
        ensure(
            !self.client_credentials.client_secret.trim().is_empty(),
            "client_credentials.client_secret must be non-empty",
        )?;

        let s = &self.streaming;
        ensure(
            (5..=60).contains(&s.sse_ping_interval_seconds),
            "streaming.sse_ping_interval_seconds must be in 5..=60",
        )?;
        ensure(
            (16..=64).contains(&s.sse_channel_capacity),
            "streaming.sse_channel_capacity must be in 16..=64",
        )?;
        ensure(
            s.max_output_tokens > 0,
            "streaming.max_output_tokens must be > 0",
        )?;

        let floor = self.estimation_budgets.minimal_generation_floor;
        ensure(
            floor > 0 && floor <= s.max_output_tokens,
            "estimation_budgets.minimal_generation_floor must be > 0 and <= streaming.max_output_tokens",
        )?;

        let q = &self.quota;
        ensure(
            (1.0..=1.5).contains(&q.overshoot_tolerance_factor),
            "quota.overshoot_tolerance_factor must be in 1.0..=1.5",
        )?;
        ensure(
            (1..=99).contains(&q.warning_threshold_pct),
            "quota.warning_threshold_pct must be in 1..=99",
        )?;
        ensure(
            q.web_search_max_calls_per_message > 0,
            "quota.web_search_max_calls_per_message must be > 0",
        )?;
        ensure(
            q.web_search_daily_quota > 0,
            "quota.web_search_daily_quota must be > 0",
        )?;
        ensure(
            q.code_interpreter_max_calls_per_message > 0,
            "quota.code_interpreter_max_calls_per_message must be > 0",
        )?;
        ensure(
            q.code_interpreter_daily_quota > 0,
            "quota.code_interpreter_daily_quota must be > 0",
        )?;

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
            ensure(
                !value.trim().is_empty(),
                format!("{name} must be non-empty"),
            )?;
        }
        ensure(
            (1..=64).contains(&o.num_partitions) && o.num_partitions.is_power_of_two(),
            "outbox.num_partitions must be a power of 2 in 1..=64",
        )?;

        ensure(
            self.context.recent_messages_limit <= 100,
            "context.recent_messages_limit must be in 0..=100",
        )?;

        let r = &self.rag;
        ensure(
            r.max_documents_per_chat > 0,
            "rag.max_documents_per_chat must be > 0",
        )?;
        ensure(
            r.max_total_upload_mb_per_chat > 0,
            "rag.max_total_upload_mb_per_chat must be > 0",
        )?;
        ensure(
            (1..=256).contains(&r.max_concurrent_uploads),
            "rag.max_concurrent_uploads must be in 1..=256",
        )?;
        ensure(
            r.uploaded_file_max_size_kb > 0,
            "rag.uploaded_file_max_size_kb must be > 0",
        )?;
        ensure(
            r.uploaded_image_max_size_kb > 0,
            "rag.uploaded_image_max_size_kb must be > 0",
        )?;
        ensure(
            r.max_images_per_message > 0,
            "rag.max_images_per_message must be > 0",
        )?;

        let t = &self.thumbnail;
        ensure(
            t.width > 0
                && t.height > 0
                && t.max_bytes > 0
                && t.max_pixels > 0
                && t.max_decode_bytes > 0,
            "thumbnail.* values must be > 0",
        )?;

        self.validate_providers()?;

        let w = &self.orphan_watchdog;
        ensure(
            (90..=3600).contains(&w.timeout_secs),
            "orphan_watchdog.timeout_secs must be in 90..=3600",
        )?;
        ensure(
            (1..=3600).contains(&w.scan_interval_secs),
            "orphan_watchdog.scan_interval_secs must be in 1..=3600",
        )?;

        let u = &self.upload_reaper;
        ensure(
            (1..=3600).contains(&u.scan_interval_secs),
            "upload_reaper.scan_interval_secs must be in 1..=3600",
        )?;
        ensure(
            (60..=86_400).contains(&u.stale_after_secs),
            "upload_reaper.stale_after_secs must be in 60..=86400",
        )?;

        let ts = &self.thread_summary_worker;
        ensure(
            (30..=3600).contains(&ts.claim_timeout_secs),
            "thread_summary_worker.claim_timeout_secs must be in 30..=3600",
        )?;
        ensure(
            ts.max_attempts > 0,
            "thread_summary_worker.max_attempts must be > 0",
        )?;
        ensure(
            (1..=99).contains(&ts.compression_threshold_pct),
            "thread_summary_worker.compression_threshold_pct must be in 1..=99",
        )?;

        ensure(
            self.cleanup_worker.max_attempts > 0,
            "cleanup_worker.max_attempts must be > 0",
        )?;

        let k = &self.knowledge_search;
        if k.enabled {
            ensure(
                k.vector_store_id
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty()),
                "knowledge_search.vector_store_id is required when knowledge_search.enabled",
            )?;
            ensure(
                k.provider_id
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty()),
                "knowledge_search.provider_id is required when knowledge_search.enabled",
            )?;
        }
        ensure(
            k.max_calls_per_message > 0,
            "knowledge_search.max_calls_per_message must be > 0",
        )?;
        ensure(k.top_k > 0, "knowledge_search.top_k must be > 0")?;
        ensure(
            k.max_chunk_chars > 0,
            "knowledge_search.max_chunk_chars must be > 0",
        )?;
        Ok(())
    }

    fn validate_providers(&self) -> Result<(), String> {
        for (id, entry) in &self.providers {
            ensure(
                is_valid_host(&entry.host),
                format!(
                    "providers.{id}.host must be non-empty and contain only letters, digits, '.', '-', '_', ':', '[', ']'"
                ),
            )?;
            ensure(
                entry.port != Some(0),
                format!("providers.{id}.port must not be 0"),
            )?;
            ensure(
                entry.api_path.starts_with('/'),
                format!("providers.{id}.api_path must start with '/'"),
            )?;
            if let Some(alias) = &entry.upstream_alias {
                ensure(
                    is_valid_host(alias),
                    format!("providers.{id}.upstream_alias contains invalid characters"),
                )?;
            }
            if entry.storage_kind == Some(StorageKind::Azure) {
                let version = entry.api_version.as_deref().unwrap_or("").trim();
                ensure(
                    !version.is_empty(),
                    format!("providers.{id}.api_version is required for storage_kind = azure"),
                )?;
            }
            if let Some(v) = &entry.api_version {
                ensure(
                    v.chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')),
                    format!(
                        "providers.{id}.api_version may contain only letters, digits, '.' and '-'"
                    ),
                )?;
            }
            if let Some(rag) = &entry.rag_provider {
                let target = self.providers.get(rag).ok_or_else(|| {
                    format!("providers.{id}.rag_provider references unknown provider '{rag}'")
                })?;
                ensure(
                    target.storage_kind.is_some(),
                    format!("providers.{id}.rag_provider '{rag}' has no storage_kind"),
                )?;
            } else {
                ensure(
                    entry.storage_kind.is_some(),
                    format!("providers.{id}.storage_kind is required (or set rag_provider)"),
                )?;
            }
            for (tenant, ov) in &entry.tenant_overrides {
                ensure(
                    uuid::Uuid::parse_str(tenant).is_ok(),
                    format!("providers.{id}.tenant_overrides key '{tenant}' must be a tenant UUID"),
                )?;
                ensure(
                    ov.host.is_some() || ov.upstream_alias.is_some(),
                    format!(
                        "providers.{id}.tenant_overrides.{tenant} must set host or upstream_alias"
                    ),
                )?;
                if let Some(h) = &ov.host {
                    ensure(
                        is_valid_host(h),
                        format!("providers.{id}.tenant_overrides.{tenant}.host is invalid"),
                    )?;
                }
            }
        }
        if self.knowledge_search.enabled
            && let Some(pid) = &self.knowledge_search.provider_id
        {
            ensure(
                self.providers.contains_key(pid),
                format!("knowledge_search.provider_id references unknown provider '{pid}'"),
            )?;
        }
        Ok(())
    }

    /// Warnings for deprecated fields set to non-default values.
    #[must_use]
    pub fn deprecation_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let d = EstimationBudgetsConfig::default();
        let e = &self.estimation_budgets;
        for (name, value, default) in [
            (
                "bytes_per_token_conservative",
                e.bytes_per_token_conservative,
                d.bytes_per_token_conservative,
            ),
            (
                "fixed_overhead_tokens",
                e.fixed_overhead_tokens,
                d.fixed_overhead_tokens,
            ),
            (
                "safety_margin_pct",
                e.safety_margin_pct,
                d.safety_margin_pct,
            ),
            (
                "image_token_budget",
                e.image_token_budget,
                d.image_token_budget,
            ),
            (
                "tool_surcharge_tokens",
                e.tool_surcharge_tokens,
                d.tool_surcharge_tokens,
            ),
            (
                "web_search_surcharge_tokens",
                e.web_search_surcharge_tokens,
                d.web_search_surcharge_tokens,
            ),
            (
                "code_interpreter_surcharge_tokens",
                e.code_interpreter_surcharge_tokens,
                d.code_interpreter_surcharge_tokens,
            ),
        ] {
            if value != default {
                out.push(format!(
                    "estimation_budgets.{name} is deprecated and has no effect (the model catalog estimation_budgets apply)"
                ));
            }
        }
        let c = CleanupWorkerConfig::default();
        let w = &self.cleanup_worker;
        if !w.enabled {
            out.push("cleanup_worker.enabled is deprecated and has no effect".to_owned());
        }
        for (name, value, default) in [
            (
                "poll_interval_secs",
                w.poll_interval_secs,
                c.poll_interval_secs,
            ),
            (
                "reconcile_interval_secs",
                w.reconcile_interval_secs,
                c.reconcile_interval_secs,
            ),
            (
                "stale_in_progress_timeout_secs",
                w.stale_in_progress_timeout_secs,
                c.stale_in_progress_timeout_secs,
            ),
            (
                "batch_size",
                u64::from(w.batch_size),
                u64::from(c.batch_size),
            ),
        ] {
            if value != default {
                out.push(format!(
                    "cleanup_worker.{name} is deprecated and has no effect"
                ));
            }
        }
        if self.thread_summary_worker.reconcile_interval_secs != 60 {
            out.push(
                "thread_summary_worker.reconcile_interval_secs is deprecated and has no effect"
                    .to_owned(),
            );
        }
        out
    }

    /// Effective metric prefix (`metrics.prefix`, default `mini_chat`).
    #[must_use]
    pub fn metrics_prefix(&self) -> &str {
        if self.metrics.prefix.trim().is_empty() {
            "mini_chat"
        } else {
            self.metrics.prefix.trim()
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
