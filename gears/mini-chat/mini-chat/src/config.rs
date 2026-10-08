//! Gear configuration (`gears.mini-chat.config`).
//!
//! Unknown keys are rejected at the top level and in the strict sections;
//! the worker sections (`orphan_watchdog`, `upload_reaper`,
//! `thread_summary_worker`, `cleanup_worker`) accept and ignore unknown keys.

use std::collections::BTreeMap;

use serde::Deserialize;

pub const DEFAULT_URL_PREFIX: &str = "/mini-chat";
pub const DEFAULT_VENDOR: &str = "constructorfabric";
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search to look up information in the documents the user attached to this chat when the question refers to them. Base answers about the documents on the retrieved excerpts.";
pub const DEFAULT_KNOWLEDGE_GUARD: &str = "Use search_knowledge to look up information in the organization knowledge base when the question needs it. Answer from the returned excerpts.";
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

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
                auth_config: Some(auth_config),
                storage_kind: Some(StorageKind::Openai),
                storage_backend: None,
                api_version: None,
                rag_provider: None,
                tenant_overrides: BTreeMap::new(),
            },
        );
        Self {
            url_prefix: DEFAULT_URL_PREFIX.to_owned(),
            vendor: DEFAULT_VENDOR.to_owned(),
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
    pub auth_config: Option<BTreeMap<String, String>>,
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
            max_output_tokens: 32_768,
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
            guard: DEFAULT_KNOWLEDGE_GUARD.to_owned(),
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

fn host_chars_ok(host: &str) -> bool {
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
}

fn expand(value: &str) -> Result<String, String> {
    toolkit::var_expand::expand_env_vars(value).map_err(|e| e.to_string())
}

impl MiniChatConfig {
    /// Expand `${VAR}` placeholders in provider `host`/`auth_config`
    /// (including tenant overrides) and in `client_credentials`.
    ///
    /// # Errors
    /// Returns a description of the first placeholder that cannot be expanded.
    pub fn expand_vars(&mut self) -> Result<(), String> {
        if let Some(cc) = self.client_credentials.as_mut() {
            cc.client_id = expand(&cc.client_id)?;
            cc.client_secret = expand(&cc.client_secret)?;
        }
        for (id, p) in &mut self.providers {
            p.host = expand(&p.host).map_err(|e| format!("providers.{id}.host: {e}"))?;
            if let Some(ac) = p.auth_config.as_mut() {
                for (k, v) in ac.iter_mut() {
                    *v = expand(v).map_err(|e| format!("providers.{id}.auth_config.{k}: {e}"))?;
                }
            }
            for (tid, o) in &mut p.tenant_overrides {
                if let Some(h) = o.host.as_mut() {
                    *h = expand(h)
                        .map_err(|e| format!("providers.{id}.tenant_overrides.{tid}.host: {e}"))?;
                }
                if let Some(ac) = o.auth_config.as_mut() {
                    for (k, v) in ac.iter_mut() {
                        *v = expand(v).map_err(|e| {
                            format!("providers.{id}.tenant_overrides.{tid}.auth_config.{k}: {e}")
                        })?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Validate every section; returns a descriptive error on the first
    /// violation.
    ///
    /// # Errors
    /// Returns the description of the invalid setting.
    #[allow(clippy::too_many_lines)]
    #[allow(
        clippy::many_single_char_names,
        reason = "short section aliases in validation"
    )]
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.trim().is_empty() {
            return Err("vendor must not be empty".into());
        }
        if !self.url_prefix.starts_with('/') {
            return Err("url_prefix must start with '/'".into());
        }
        if let Some(cc) = &self.client_credentials
            && (cc.client_id.trim().is_empty() || cc.client_secret.trim().is_empty())
        {
            return Err("client_credentials.client_id and client_secret must be non-empty".into());
        }
        let s = &self.streaming;
        if !(5..=60).contains(&s.sse_ping_interval_seconds) {
            return Err("streaming.sse_ping_interval_seconds must be in 5..=60".into());
        }
        if !(16..=64).contains(&s.sse_channel_capacity) {
            return Err("streaming.sse_channel_capacity must be in 16..=64".into());
        }
        if s.max_output_tokens == 0 {
            return Err("streaming.max_output_tokens must be > 0".into());
        }
        let floor = self.estimation_budgets.minimal_generation_floor;
        if floor == 0 || floor > s.max_output_tokens {
            return Err(
                "estimation_budgets.minimal_generation_floor must be > 0 and <= streaming.max_output_tokens"
                    .into(),
            );
        }
        let q = &self.quota;
        if !(1.0..=1.5).contains(&q.overshoot_tolerance_factor) {
            return Err("quota.overshoot_tolerance_factor must be in 1.0..=1.5".into());
        }
        if !(1..=99).contains(&q.warning_threshold_pct) {
            return Err("quota.warning_threshold_pct must be in 1..=99".into());
        }
        if q.web_search_max_calls_per_message == 0
            || q.web_search_daily_quota == 0
            || q.code_interpreter_max_calls_per_message == 0
            || q.code_interpreter_daily_quota == 0
        {
            return Err("quota call limits must be > 0".into());
        }
        let o = &self.outbox;
        for (name, v) in [
            ("queue_name", &o.queue_name),
            ("cleanup_queue_name", &o.cleanup_queue_name),
            ("chat_cleanup_queue_name", &o.chat_cleanup_queue_name),
            ("thread_summary_queue_name", &o.thread_summary_queue_name),
            ("audit_queue_name", &o.audit_queue_name),
        ] {
            if v.trim().is_empty() {
                return Err(format!("outbox.{name} must not be empty"));
            }
        }
        if !(1..=64).contains(&o.num_partitions) || !o.num_partitions.is_power_of_two() {
            return Err("outbox.num_partitions must be a power of two in 1..=64".into());
        }
        if self.context.recent_messages_limit > 100 {
            return Err("context.recent_messages_limit must be in 0..=100".into());
        }
        let r = &self.rag;
        if r.max_documents_per_chat == 0
            || r.max_total_upload_mb_per_chat == 0
            || r.uploaded_file_max_size_kb == 0
            || r.uploaded_image_max_size_kb == 0
            || r.max_images_per_message == 0
        {
            return Err("rag limits must be > 0".into());
        }
        if !(1..=256).contains(&r.max_concurrent_uploads) {
            return Err("rag.max_concurrent_uploads must be in 1..=256".into());
        }
        let t = &self.thumbnail;
        if t.width == 0
            || t.height == 0
            || t.max_bytes == 0
            || t.max_pixels == 0
            || t.max_decode_bytes == 0
        {
            return Err("thumbnail settings must be > 0".into());
        }
        let k = &self.knowledge_search;
        if k.enabled {
            if k.vector_store_id
                .as_deref()
                .is_none_or(|v| v.trim().is_empty())
            {
                return Err("knowledge_search.vector_store_id is required when enabled".into());
            }
            if k.provider_id.as_deref().is_none_or(|v| v.trim().is_empty()) {
                return Err("knowledge_search.provider_id is required when enabled".into());
            }
        }
        if k.max_calls_per_message == 0 || k.top_k == 0 || k.max_chunk_chars == 0 {
            return Err("knowledge_search limits must be > 0".into());
        }
        let w = &self.orphan_watchdog;
        if !(90..=3600).contains(&w.timeout_secs) {
            return Err("orphan_watchdog.timeout_secs must be in 90..=3600".into());
        }
        if !(1..=3600).contains(&w.scan_interval_secs) {
            return Err("orphan_watchdog.scan_interval_secs must be in 1..=3600".into());
        }
        let u = &self.upload_reaper;
        if !(1..=3600).contains(&u.scan_interval_secs) {
            return Err("upload_reaper.scan_interval_secs must be in 1..=3600".into());
        }
        if !(60..=86_400).contains(&u.stale_after_secs) {
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
        for (id, p) in &self.providers {
            if !host_chars_ok(&p.host) {
                return Err(format!(
                    "providers.{id}.host is empty or contains invalid characters"
                ));
            }
            if p.port == Some(0) {
                return Err(format!("providers.{id}.port must not be 0"));
            }
            if p.storage_kind.is_none() {
                return Err(format!("providers.{id}.storage_kind is required"));
            }
            if p.storage_kind == Some(StorageKind::Azure) {
                match p.api_version.as_deref().map(str::trim) {
                    Some(v)
                        if !v.is_empty()
                            && v.chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') => {}
                    _ => {
                        return Err(format!(
                            "providers.{id}.api_version is required for storage_kind azure and may only contain letters, digits, '.' and '-'"
                        ));
                    }
                }
            }
            if !p.api_path.starts_with('/') {
                return Err(format!("providers.{id}.api_path must start with '/'"));
            }
            if let Some(rp) = &p.rag_provider
                && !self.providers.contains_key(rp)
            {
                return Err(format!(
                    "providers.{id}.rag_provider '{rp}' is not a configured provider"
                ));
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
                    return Err(format!(
                        "providers.{id}.tenant_overrides.{tid}.host contains invalid characters"
                    ));
                }
            }
        }
        if k.enabled
            && let Some(pid) = &k.provider_id
            && !self.providers.contains_key(pid)
        {
            return Err(format!(
                "knowledge_search.provider_id '{pid}' is not a configured provider"
            ));
        }
        Ok(())
    }

    /// Log a warning for every deprecated setting that is set to a
    /// non-default value.
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    pub fn warn_deprecated(&self) {
        let d = EstimationBudgetsConfig::default();
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
                tracing::warn!(field = %format!("estimation_budgets.{name}"), "deprecated setting has no effect");
            }
        }
        let c = &self.cleanup_worker;
        let cd = CleanupWorkerConfig::default();
        if !c.enabled {
            tracing::warn!(
                field = "cleanup_worker.enabled",
                "deprecated setting has no effect"
            );
        }
        if c.poll_interval_secs != cd.poll_interval_secs
            || c.reconcile_interval_secs != cd.reconcile_interval_secs
            || c.stale_in_progress_timeout_secs != cd.stale_in_progress_timeout_secs
            || c.batch_size != cd.batch_size
        {
            tracing::warn!(
                field = "cleanup_worker.*",
                "deprecated setting has no effect"
            );
        }
        if self.thread_summary_worker.reconcile_interval_secs != 60 {
            tracing::warn!(
                field = "thread_summary_worker.reconcile_interval_secs",
                "deprecated setting has no effect"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        let cfg = MiniChatConfig::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.url_prefix, "/mini-chat");
        assert_eq!(cfg.outbox.num_partitions, 4);
        assert_eq!(cfg.context.recent_messages_limit, 10);
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let res: Result<MiniChatConfig, _> =
            serde_json::from_value(serde_json::json!({"no_such_key": 1}));
        assert!(res.is_err());
    }

    #[test]
    fn removed_provider_key_is_rejected() {
        let res: Result<MiniChatConfig, _> = serde_json::from_value(serde_json::json!({
            "providers": {"p": {"kind": "openai_responses", "host": "h", "storage_kind": "openai",
                "supports_file_search_filters": true}}
        }));
        assert!(res.is_err());
    }

    #[test]
    fn worker_sections_accept_unknown_keys() {
        let cfg: MiniChatConfig = serde_json::from_value(serde_json::json!({
            "orphan_watchdog": {"enabled": true, "whatever": 1},
            "thread_summary_worker": {"foo": "bar"}
        }))
        .unwrap();
        assert!(cfg.orphan_watchdog.enabled);
    }

    #[test]
    fn invalid_ranges_fail_validation() {
        let mut cfg = MiniChatConfig::default();
        cfg.orphan_watchdog.timeout_secs = 10;
        assert!(cfg.validate().is_err());
        let mut cfg = MiniChatConfig::default();
        cfg.streaming.sse_ping_interval_seconds = 1;
        assert!(cfg.validate().is_err());
        let mut cfg = MiniChatConfig::default();
        cfg.outbox.num_partitions = 3;
        assert!(cfg.validate().is_err());
        let mut cfg = MiniChatConfig::default();
        if let Some(p) = cfg.providers.get_mut("openai") {
            p.storage_kind = Some(StorageKind::Azure);
        }
        assert!(cfg.validate().is_err());
    }
}
