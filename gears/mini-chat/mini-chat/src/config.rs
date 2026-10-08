//! Gear configuration (`gears.mini-chat.config`).
//!
//! Every section is validated at gear init ([`MiniChatConfig::validate`]).
//! Strict sections reject unknown keys; the worker sections
//! (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`,
//! `cleanup_worker`) accept and ignore them (DESIGN Appendix B.1).

use std::collections::HashMap;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use toolkit_macros::ExpandVars;

/// Default summary model when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Built-in default system prompt of the summary request (DESIGN B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Default `context.web_search_guard` (DESIGN §4 "Web Search Configuration").
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Default `context.file_search_guard`.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search only when the user's question concerns the documents attached to this chat. Cite the documents you rely on.";

/// Default `knowledge_search.guard`.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to look up organization knowledge when the question cannot be answered from the conversation. Do not call it for general knowledge questions.";

/// Root gear configuration.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    pub url_prefix: String,
    pub vendor: String,
    #[expand_vars]
    pub client_credentials: ClientCredentialsConfig,
    pub metrics: MetricsConfig,
    #[expand_vars]
    pub providers: HashMap<String, ProviderEntry>,
    pub streaming: StreamingConfig,
    pub estimation_budgets: GearEstimationBudgets,
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
            client_credentials: ClientCredentialsConfig::default(),
            metrics: MetricsConfig::default(),
            providers: default_providers(),
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

fn default_providers() -> HashMap<String, ProviderEntry> {
    let mut auth = HashMap::new();
    auth.insert("header".to_owned(), "Authorization".to_owned());
    auth.insert("prefix".to_owned(), "Bearer ".to_owned());
    auth.insert("secret_ref".to_owned(), "cred://openai-key".to_owned());
    let mut m = HashMap::new();
    m.insert(
        "openai".to_owned(),
        ProviderEntry {
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
            storage_kind: StorageKind::Openai,
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: HashMap::new(),
        },
    );
    m
}

/// S2S client credentials used for OAGW provisioning.
#[derive(Clone, Deserialize, ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCredentialsConfig {
    #[expand_vars]
    pub client_id: String,
    #[expand_vars]
    pub client_secret: SecretString,
}

impl Default for ClientCredentialsConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: SecretString::from(String::new()),
        }
    }
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

/// Provider adapter kind (ADR-0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

/// File / vector-store implementation of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

/// One `providers.<id>` entry.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    #[expand_vars]
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
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
    pub storage_kind: StorageKind,
    #[serde(default)]
    pub storage_backend: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub rag_provider: Option<String>,
    #[serde(default)]
    #[expand_vars]
    pub tenant_overrides: HashMap<String, TenantOverride>,
}

impl ProviderEntry {
    /// Effective port.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }

    /// Default upstream alias of `host` on this entry's port (see [`default_alias`]).
    #[must_use]
    pub fn default_alias_for(&self, host: &str) -> String {
        default_alias(host, self.effective_port(), self.use_http)
    }
}

/// Default upstream alias: the host (ADR-0005). For a hostname on a
/// non-standard port OAGW derives `host:port` and rejects any other alias, so
/// that value is used there; IP hosts keep the bare host.
#[must_use]
pub fn default_alias(host: &str, port: u16, use_http: bool) -> String {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let is_ip = bare.parse::<std::net::IpAddr>().is_ok();
    let standard = if use_http { port == 80 } else { port == 443 };
    if is_ip || standard {
        host.to_owned()
    } else {
        format!("{}:{port}", host.to_ascii_lowercase().trim_end_matches('.'))
    }
}

/// Per-tenant override of a provider entry.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct TenantOverride {
    #[serde(default)]
    #[expand_vars]
    pub host: Option<String>,
    #[serde(default)]
    pub upstream_alias: Option<String>,
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    #[serde(default)]
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
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

/// Gear-level estimation budgets. Only `minimal_generation_floor` is used;
/// the other fields are deprecated (ADR-0008).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GearEstimationBudgets {
    pub minimal_generation_floor: u32,
    pub bytes_per_token_conservative: u32,
    pub fixed_overhead_tokens: u32,
    pub safety_margin_pct: u32,
    pub image_token_budget: u32,
    pub tool_surcharge_tokens: u32,
    pub web_search_surcharge_tokens: u32,
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
    /// The summary model id (`gpt-4.1-mini` when empty).
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
    pub enabled: bool,
    pub poll_interval_secs: u64,
    pub reconcile_interval_secs: u64,
    pub stale_in_progress_timeout_secs: u64,
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

fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

impl MiniChatConfig {
    /// Validates every section. Returns a descriptive error on the first failure.
    ///
    /// # Errors
    /// Returns a message naming the offending key.
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.trim().is_empty() {
            return Err("vendor must be non-empty".into());
        }
        if self.client_credentials.client_id.trim().is_empty() {
            return Err("client_credentials.client_id must be non-empty".into());
        }
        if self
            .client_credentials
            .client_secret
            .expose_secret()
            .trim()
            .is_empty()
        {
            return Err("client_credentials.client_secret must be non-empty".into());
        }
        let streaming = &self.streaming;
        if !(5..=60).contains(&streaming.sse_ping_interval_seconds) {
            return Err("streaming.sse_ping_interval_seconds must be in 5..=60".into());
        }
        if !(16..=64).contains(&streaming.sse_channel_capacity) {
            return Err("streaming.sse_channel_capacity must be in 16..=64".into());
        }
        if streaming.max_output_tokens == 0 {
            return Err("streaming.max_output_tokens must be > 0".into());
        }
        let floor = self.estimation_budgets.minimal_generation_floor;
        if floor == 0 || floor > streaming.max_output_tokens {
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
            if v == 0 {
                return Err(format!("{name} must be > 0"));
            }
        }
        let outbox = &self.outbox;
        for (name, v) in [
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
            if v.trim().is_empty() {
                return Err(format!("{name} must be non-empty"));
            }
        }
        if !(1..=64).contains(&outbox.num_partitions) || !outbox.num_partitions.is_power_of_two() {
            return Err("outbox.num_partitions must be a power of 2 in 1..=64".into());
        }
        if self.context.recent_messages_limit > 100 {
            return Err("context.recent_messages_limit must be in 0..=100".into());
        }
        let rag = &self.rag;
        for (name, v) in [
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
            if v == 0 {
                return Err(format!("{name} must be > 0"));
            }
        }
        if !(1..=256).contains(&rag.max_concurrent_uploads) {
            return Err("rag.max_concurrent_uploads must be in 1..=256".into());
        }
        let thumb = &self.thumbnail;
        if thumb.width == 0
            || thumb.height == 0
            || thumb.max_bytes == 0
            || thumb.max_pixels == 0
            || thumb.max_decode_bytes == 0
        {
            return Err("thumbnail.* values must be > 0".into());
        }
        let ks = &self.knowledge_search;
        if ks.enabled {
            if ks
                .vector_store_id
                .as_deref()
                .is_none_or(|v| v.trim().is_empty())
            {
                return Err("knowledge_search.vector_store_id is required when enabled".into());
            }
            if ks
                .provider_id
                .as_deref()
                .is_none_or(|v| v.trim().is_empty())
            {
                return Err("knowledge_search.provider_id is required when enabled".into());
            }
        }
        if ks.max_calls_per_message == 0 || ks.top_k == 0 || ks.max_chunk_chars == 0 {
            return Err(
                "knowledge_search.max_calls_per_message, top_k and max_chunk_chars must be > 0"
                    .into(),
            );
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
        for (id, p) in &self.providers {
            if !is_valid_host(&p.host) {
                return Err(format!(
                    "providers.{id}.host is empty or contains invalid characters"
                ));
            }
            if p.port == Some(0) {
                return Err(format!("providers.{id}.port must not be 0"));
            }
            if !p.api_path.starts_with('/') {
                return Err(format!("providers.{id}.api_path must start with '/'"));
            }
            if p.storage_kind == StorageKind::Azure {
                let v = p.api_version.as_deref().unwrap_or("").trim();
                if v.is_empty() {
                    return Err(format!(
                        "providers.{id}.api_version is required for storage_kind azure"
                    ));
                }
            }
            if let Some(v) = p.api_version.as_deref()
                && !v
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                return Err(format!(
                    "providers.{id}.api_version contains invalid characters"
                ));
            }
            if let Some(rag) = p.rag_provider.as_deref()
                && !self.providers.contains_key(rag)
            {
                return Err(format!(
                    "providers.{id}.rag_provider references unknown provider '{rag}'"
                ));
            }
            for (tenant, o) in &p.tenant_overrides {
                if o.host.is_none() && o.upstream_alias.is_none() {
                    return Err(format!(
                        "providers.{id}.tenant_overrides.{tenant} must set host or upstream_alias"
                    ));
                }
                if let Some(h) = o.host.as_deref()
                    && !is_valid_host(h)
                {
                    return Err(format!(
                        "providers.{id}.tenant_overrides.{tenant}.host is invalid"
                    ));
                }
            }
        }
        if self.knowledge_search.enabled
            && let Some(pid) = self.knowledge_search.provider_id.as_deref()
            && !self.providers.contains_key(pid)
        {
            return Err(format!(
                "knowledge_search.provider_id references unknown provider '{pid}'"
            ));
        }
        Ok(())
    }

    /// Logs a warning for every deprecated field set to a non-default value (ADR-0008, ADR-0010).
    // Flat list of independent field checks; splitting it would not make it easier to read.
    #[allow(clippy::cognitive_complexity)]
    pub fn warn_deprecated(&self) {
        let d = GearEstimationBudgets::default();
        let e = &self.estimation_budgets;
        for (name, v, dv) in [
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
            if v != dv {
                tracing::warn!(
                    field = %format!("estimation_budgets.{name}"),
                    "deprecated configuration field has no effect (catalog estimation_budgets are used)"
                );
            }
        }
        let c = &self.cleanup_worker;
        let dc = CleanupWorkerConfig::default();
        if !c.enabled {
            tracing::warn!(
                field = "cleanup_worker.enabled",
                "deprecated configuration field has no effect"
            );
        }
        for (name, v, dv) in [
            (
                "cleanup_worker.poll_interval_secs",
                c.poll_interval_secs,
                dc.poll_interval_secs,
            ),
            (
                "cleanup_worker.reconcile_interval_secs",
                c.reconcile_interval_secs,
                dc.reconcile_interval_secs,
            ),
            (
                "cleanup_worker.stale_in_progress_timeout_secs",
                c.stale_in_progress_timeout_secs,
                dc.stale_in_progress_timeout_secs,
            ),
            (
                "cleanup_worker.batch_size",
                u64::from(c.batch_size),
                u64::from(dc.batch_size),
            ),
            (
                "thread_summary_worker.reconcile_interval_secs",
                self.thread_summary_worker.reconcile_interval_secs,
                ThreadSummaryWorkerConfig::default().reconcile_interval_secs,
            ),
        ] {
            if v != dv {
                tracing::warn!(field = name, "deprecated configuration field has no effect");
            }
        }
    }

    /// Fills `upstream_alias` with the host where it is not configured (ADR-0005).
    pub fn fill_upstream_aliases(&mut self) {
        for p in self.providers.values_mut() {
            if p.upstream_alias
                .as_deref()
                .is_none_or(|a| a.trim().is_empty())
            {
                p.upstream_alias = Some(p.default_alias_for(&p.host));
            }
            let (port, use_http) = (p.effective_port(), p.use_http);
            for o in p.tenant_overrides.values_mut() {
                if o.upstream_alias
                    .as_deref()
                    .is_none_or(|a| a.trim().is_empty())
                {
                    o.upstream_alias = o.host.as_deref().map(|h| default_alias(h, port, use_http));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
