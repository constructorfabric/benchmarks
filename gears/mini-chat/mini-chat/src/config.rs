//! Gear configuration (DESIGN Appendix B).
//!
//! Unknown keys are rejected at the top level and in the sections listed in
//! B.1; the worker sections accept (and ignore) unknown keys.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Error produced by configuration validation.
#[derive(Debug, thiserror::Error)]
#[error("invalid mini-chat configuration: {0}")]
pub struct ConfigError(pub String);

fn err(msg: impl Into<String>) -> ConfigError {
    ConfigError(msg.into())
}

pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "The user has uploaded documents to this chat. Use file_search to look up information in these documents when the question relates to their content. Cite the documents you use.";
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to look up information in the organization knowledge base when the question relates to internal knowledge. Do not call it for general knowledge questions.";
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Top-level gear configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MiniChatConfig {
    pub url_prefix: String,
    pub vendor: String,
    pub client_credentials: ClientCredentialsConfig,
    pub metrics: MetricsConfig,
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
        let mut providers = BTreeMap::new();
        let mut auth_config = BTreeMap::new();
        auth_config.insert("header".to_owned(), "Authorization".to_owned());
        auth_config.insert("prefix".to_owned(), "Bearer ".to_owned());
        auth_config.insert("secret_ref".to_owned(), "cred://openai-key".to_owned());
        providers.insert(
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
                auth_config,
                storage_kind: Some(StorageKind::Openai),
                storage_backend: None,
                api_version: None,
                rag_provider: None,
                tenant_overrides: BTreeMap::new(),
            },
        );
        Self {
            url_prefix: "/mini-chat".to_owned(),
            vendor: "constructorfabric".to_owned(),
            client_credentials: ClientCredentialsConfig::default(),
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

#[derive(Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    pub prefix: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

/// `providers.<id>` entry (ADR-0005).
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
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }

    /// Alias under which the upstream is registered (configured alias or host).
    #[must_use]
    pub fn alias(&self) -> String {
        self.upstream_alias.clone().unwrap_or_else(|| self.host.clone())
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
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

/// Gear-level estimation budgets: only `minimal_generation_floor` is used.
#[derive(Debug, Clone, Deserialize)]
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
            uploaded_file_max_size_kb: 25600,
            uploaded_image_max_size_kb: 5120,
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
    /// Summary model id (`""` = `gpt-4.1-mini`).
    #[must_use]
    pub fn summary_model(&self) -> &str {
        if self.summary_model_id.trim().is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            &self.summary_model_id
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

fn valid_host_chars(host: &str) -> bool {
    host.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

fn expand(value: &str, field: &str) -> Result<String, ConfigError> {
    toolkit_utils::var_expand::expand_env_vars(value)
        .map_err(|e| err(format!("{field}: {e}")))
}

impl MiniChatConfig {
    /// Parse the raw `gears.mini-chat.config` value (absent → defaults),
    /// expand `${VAR}` placeholders, fill derived defaults and validate.
    ///
    /// # Errors
    /// Unknown keys, invalid values or unresolvable placeholders.
    pub fn from_value(value: Option<&serde_json::Value>) -> Result<Self, ConfigError> {
        let mut cfg: Self = match value {
            None | Some(serde_json::Value::Null) => Self::default(),
            Some(v) => serde_json::from_value(v.clone()).map_err(|e| err(e.to_string()))?,
        };
        cfg.expand_vars()?;
        cfg.fill_defaults();
        cfg.validate()?;
        Ok(cfg)
    }

    fn expand_vars(&mut self) -> Result<(), ConfigError> {
        self.client_credentials.client_id =
            expand(&self.client_credentials.client_id, "client_credentials.client_id")?;
        self.client_credentials.client_secret = expand(
            &self.client_credentials.client_secret,
            "client_credentials.client_secret",
        )?;
        for (id, p) in &mut self.providers {
            p.host = expand(&p.host, &format!("providers.{id}.host"))?;
            for v in p.auth_config.values_mut() {
                *v = expand(v, &format!("providers.{id}.auth_config"))?;
            }
            for (tid, o) in &mut p.tenant_overrides {
                if let Some(h) = &o.host {
                    o.host = Some(expand(h, &format!("providers.{id}.tenant_overrides.{tid}.host"))?);
                }
                if let Some(ac) = &mut o.auth_config {
                    for v in ac.values_mut() {
                        *v = expand(v, &format!("providers.{id}.tenant_overrides.{tid}.auth_config"))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn fill_defaults(&mut self) {
        for p in self.providers.values_mut() {
            if p.upstream_alias.as_deref().is_none_or(|a| a.trim().is_empty()) {
                p.upstream_alias = Some(p.host.clone());
            }
            for o in p.tenant_overrides.values_mut() {
                if o.upstream_alias.as_deref().is_none_or(|a| a.trim().is_empty()) {
                    o.upstream_alias.clone_from(&o.host);
                }
            }
        }
    }

    /// Validate every section.
    ///
    /// # Errors
    /// First violated rule.
    // reason: flat list of independent rules; short section aliases (`s`, `q`, `o`, ...) keep it readable
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity, clippy::many_single_char_names)]
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.vendor.trim().is_empty() {
            return Err(err("vendor must be non-empty"));
        }
        if !self.url_prefix.starts_with('/') || self.url_prefix.ends_with('/') && self.url_prefix.len() > 1 {
            return Err(err("url_prefix must start with '/' and not end with '/'"));
        }
        if self.client_credentials.client_id.trim().is_empty()
            || self.client_credentials.client_secret.trim().is_empty()
        {
            return Err(err(
                "client_credentials.client_id and client_credentials.client_secret are required",
            ));
        }
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
        let floor = self.estimation_budgets.minimal_generation_floor;
        if floor == 0 || floor > s.max_output_tokens {
            return Err(err(
                "estimation_budgets.minimal_generation_floor must be > 0 and <= streaming.max_output_tokens",
            ));
        }
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
        let o = &self.outbox;
        for (name, v) in [
            ("outbox.queue_name", &o.queue_name),
            ("outbox.cleanup_queue_name", &o.cleanup_queue_name),
            ("outbox.chat_cleanup_queue_name", &o.chat_cleanup_queue_name),
            ("outbox.thread_summary_queue_name", &o.thread_summary_queue_name),
            ("outbox.audit_queue_name", &o.audit_queue_name),
        ] {
            if v.trim().is_empty() {
                return Err(err(format!("{name} must be non-empty")));
            }
        }
        if !(1..=64).contains(&o.num_partitions) || !o.num_partitions.is_power_of_two() {
            return Err(err("outbox.num_partitions must be a power of 2 in 1..=64"));
        }
        if self.context.recent_messages_limit > 100 {
            return Err(err("context.recent_messages_limit must be in 0..=100"));
        }
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
        let t = &self.thumbnail;
        if t.width == 0 || t.height == 0 || t.max_bytes == 0 || t.max_pixels == 0 || t.max_decode_bytes == 0 {
            return Err(err("thumbnail values must be > 0"));
        }
        let k = &self.knowledge_search;
        if k.enabled {
            if k.vector_store_id.as_deref().is_none_or(|v| v.trim().is_empty()) {
                return Err(err("knowledge_search.vector_store_id is required when enabled"));
            }
            if k.provider_id.as_deref().is_none_or(|v| v.trim().is_empty()) {
                return Err(err("knowledge_search.provider_id is required when enabled"));
            }
        }
        if k.max_calls_per_message == 0 || k.top_k == 0 || k.max_chunk_chars == 0 {
            return Err(err("knowledge_search limits must be > 0"));
        }
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
        if !(60..=86400).contains(&u.stale_after_secs) {
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
        self.validate_providers()
    }

    fn validate_providers(&self) -> Result<(), ConfigError> {
        for (id, p) in &self.providers {
            if p.host.trim().is_empty() {
                return Err(err(format!("providers.{id}.host must be non-empty")));
            }
            if !valid_host_chars(&p.host) {
                return Err(err(format!("providers.{id}.host contains invalid characters")));
            }
            if p.port == Some(0) {
                return Err(err(format!("providers.{id}.port must not be 0")));
            }
            if !p.api_path.starts_with('/') {
                return Err(err(format!("providers.{id}.api_path must start with '/'")));
            }
            if p.storage_kind == Some(StorageKind::Azure) {
                let v = p.api_version.as_deref().unwrap_or("").trim();
                if v.is_empty() {
                    return Err(err(format!(
                        "providers.{id}.api_version is required for storage_kind azure"
                    )));
                }
            }
            if let Some(v) = &p.api_version
                && !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                return Err(err(format!("providers.{id}.api_version has invalid characters")));
            }
            if let Some(rag) = &p.rag_provider
                && !self.providers.contains_key(rag)
            {
                return Err(err(format!(
                    "providers.{id}.rag_provider '{rag}' does not name a provider"
                )));
            }
            for (tid, o) in &p.tenant_overrides {
                if o.host.is_none() && o.upstream_alias.is_none() {
                    return Err(err(format!(
                        "providers.{id}.tenant_overrides.{tid} must set host or upstream_alias"
                    )));
                }
                if let Some(h) = &o.host
                    && (h.trim().is_empty() || !valid_host_chars(h))
                {
                    return Err(err(format!(
                        "providers.{id}.tenant_overrides.{tid}.host is invalid"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Names of deprecated fields set to a non-default value (logged at startup).
    #[must_use]
    pub fn deprecated_field_warnings(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let d = EstimationBudgetsConfig::default();
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
mod tests;
