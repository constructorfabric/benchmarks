//! Gear configuration (DESIGN Appendix B). Unknown keys are rejected at the top level and in
//! the strict sections; the worker sections accept and ignore unknown keys.

use std::collections::HashMap;

use serde::Deserialize;

/// Built-in default of `context.web_search_guard`.
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";
/// Built-in default of `context.file_search_guard`.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search to find relevant passages in the documents attached to this chat when the question refers to them. Answer from the retrieved passages and do not invent document content.";
/// Built-in default of `knowledge_search.guard`.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use the search_knowledge tool to look up organization knowledge when the question needs it.";
/// Built-in summary system prompt (B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";
/// Default summary model when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    pub url_prefix: String,
    pub vendor: String,
    pub client_credentials: ClientCredentialsConfig,
    pub metrics: MetricsConfig,
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCredentialsConfig {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub prefix: String,
}

/// Adapter kind of a provider entry (ADR-0005).
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
    pub storage_kind: StorageKind,
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
            storage_kind: StorageKind::Openai,
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: HashMap::new(),
        }
    }

    /// Effective port: configured, else 80 for HTTP and 443 for HTTPS.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port
            .unwrap_or(if self.use_http { 80 } else { 443 })
    }

    /// OAGW alias: configured `upstream_alias`, else the host (with `:port` on a
    /// non-standard port, matching OAGW alias derivation).
    #[must_use]
    pub fn alias(&self) -> String {
        if let Some(a) = self.upstream_alias.as_ref().filter(|a| !a.trim().is_empty()) {
            return a.clone();
        }
        derive_alias(&self.host, self.effective_port())
    }

    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`.
    #[must_use]
    pub fn storage_backend_label(&self, provider_id: &str) -> String {
        self.storage_backend
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| provider_id.to_owned())
    }
}

/// Alias OAGW derives for a single hostname endpoint.
#[must_use]
pub fn derive_alias(host: &str, port: u16) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if port == 80 || port == 443 {
        host
    } else {
        format!("{host}:{port}")
    }
}

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

/// Gear-level estimation budgets: only `minimal_generation_floor` is used; the rest is
/// deprecated (parsed, not validated, warned when non-default).
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
    /// Effective summary model id (`gpt-4.1-mini` when empty).
    #[must_use]
    pub fn effective_model_id(&self) -> &str {
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

/// Expands `${VAR}` / `${VAR:-default}` references. A missing variable without default is an error.
///
/// # Errors
/// Returns the name of the missing variable.
pub fn expand_vars(input: &str) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let expr = &after[..end];
        let (name, default) = match expr.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (expr, None),
        };
        match std::env::var(name) {
            Ok(v) if !(v.is_empty() && default.is_some()) => out.push_str(&v),
            _ => match default {
                Some(d) => out.push_str(d),
                None => return Err(format!("environment variable '{name}' is not set")),
            },
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn expand_map(map: &mut Option<HashMap<String, String>>) -> Result<(), String> {
    if let Some(m) = map.as_mut() {
        for v in m.values_mut() {
            *v = expand_vars(v)?;
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

fn is_power_of_two_in(n: u32, max: u32) -> bool {
    (1..=max).contains(&n) && n.is_power_of_two()
}

impl MiniChatConfig {
    /// Applies `${VAR}` expansion to provider hosts, auth configs and client credentials.
    ///
    /// # Errors
    /// Returns a description of the first expansion failure.
    pub fn expand(&mut self) -> Result<(), String> {
        self.client_credentials.client_id = expand_vars(&self.client_credentials.client_id)?;
        self.client_credentials.client_secret =
            expand_vars(&self.client_credentials.client_secret)?;
        for (id, p) in &mut self.providers {
            p.host = expand_vars(&p.host).map_err(|e| format!("providers.{id}.host: {e}"))?;
            expand_map(&mut p.auth_config).map_err(|e| format!("providers.{id}.auth_config: {e}"))?;
            for (tid, o) in &mut p.tenant_overrides {
                if let Some(h) = o.host.as_mut() {
                    *h = expand_vars(h)
                        .map_err(|e| format!("providers.{id}.tenant_overrides.{tid}.host: {e}"))?;
                }
                expand_map(&mut o.auth_config).map_err(|e| {
                    format!("providers.{id}.tenant_overrides.{tid}.auth_config: {e}")
                })?;
            }
        }
        Ok(())
    }

    /// Validates every section (gear init).
    ///
    /// # Errors
    /// Returns a description of the first invalid value.
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.trim().is_empty() {
            return Err("vendor must not be empty".to_owned());
        }
        if !self.url_prefix.starts_with('/') {
            return Err("url_prefix must start with '/'".to_owned());
        }
        if self.client_credentials.client_id.trim().is_empty()
            || self.client_credentials.client_secret.is_empty()
        {
            return Err("client_credentials.client_id and client_secret are required".to_owned());
        }
        let s = &self.streaming;
        if !(5..=60).contains(&s.sse_ping_interval_seconds) {
            return Err("streaming.sse_ping_interval_seconds must be in 5..=60".to_owned());
        }
        if !(16..=64).contains(&s.sse_channel_capacity) {
            return Err("streaming.sse_channel_capacity must be in 16..=64".to_owned());
        }
        if s.max_output_tokens == 0 {
            return Err("streaming.max_output_tokens must be > 0".to_owned());
        }
        let floor = self.estimation_budgets.minimal_generation_floor;
        if floor == 0 || floor > s.max_output_tokens {
            return Err(
                "estimation_budgets.minimal_generation_floor must be > 0 and <= streaming.max_output_tokens"
                    .to_owned(),
            );
        }
        let q = &self.quota;
        if !(1.0..=1.5).contains(&q.overshoot_tolerance_factor) {
            return Err("quota.overshoot_tolerance_factor must be in 1.0..=1.5".to_owned());
        }
        if !(1..=99).contains(&q.warning_threshold_pct) {
            return Err("quota.warning_threshold_pct must be in 1..=99".to_owned());
        }
        if q.web_search_max_calls_per_message == 0
            || q.web_search_daily_quota == 0
            || q.code_interpreter_max_calls_per_message == 0
            || q.code_interpreter_daily_quota == 0
        {
            return Err("quota call limits must be > 0".to_owned());
        }
        let o = &self.outbox;
        for (k, v) in [
            ("queue_name", &o.queue_name),
            ("cleanup_queue_name", &o.cleanup_queue_name),
            ("chat_cleanup_queue_name", &o.chat_cleanup_queue_name),
            ("thread_summary_queue_name", &o.thread_summary_queue_name),
            ("audit_queue_name", &o.audit_queue_name),
        ] {
            if v.trim().is_empty() {
                return Err(format!("outbox.{k} must not be empty"));
            }
        }
        if !is_power_of_two_in(o.num_partitions, 64) {
            return Err("outbox.num_partitions must be a power of two in 1..=64".to_owned());
        }
        if self.context.recent_messages_limit > 100 {
            return Err("context.recent_messages_limit must be in 0..=100".to_owned());
        }
        let r = &self.rag;
        if r.max_documents_per_chat == 0
            || r.max_total_upload_mb_per_chat == 0
            || r.uploaded_file_max_size_kb == 0
            || r.uploaded_image_max_size_kb == 0
            || r.max_images_per_message == 0
        {
            return Err("rag limits must be > 0".to_owned());
        }
        if !(1..=256).contains(&r.max_concurrent_uploads) {
            return Err("rag.max_concurrent_uploads must be in 1..=256".to_owned());
        }
        let t = &self.thumbnail;
        if t.width == 0 || t.height == 0 || t.max_bytes == 0 || t.max_pixels == 0 || t.max_decode_bytes == 0
        {
            return Err("thumbnail values must be > 0".to_owned());
        }
        let ks = &self.knowledge_search;
        if ks.enabled
            && (ks.vector_store_id.as_deref().is_none_or(str::is_empty)
                || ks.provider_id.as_deref().is_none_or(str::is_empty))
        {
            return Err(
                "knowledge_search.vector_store_id and provider_id are required when enabled".to_owned(),
            );
        }
        if ks.max_calls_per_message == 0 || ks.top_k == 0 || ks.max_chunk_chars == 0 {
            return Err("knowledge_search limits must be > 0".to_owned());
        }
        let w = &self.orphan_watchdog;
        if !(90..=3600).contains(&w.timeout_secs) {
            return Err("orphan_watchdog.timeout_secs must be in 90..=3600".to_owned());
        }
        if !(1..=3600).contains(&w.scan_interval_secs) {
            return Err("orphan_watchdog.scan_interval_secs must be in 1..=3600".to_owned());
        }
        let u = &self.upload_reaper;
        if !(1..=3600).contains(&u.scan_interval_secs) {
            return Err("upload_reaper.scan_interval_secs must be in 1..=3600".to_owned());
        }
        if !(60..=86_400).contains(&u.stale_after_secs) {
            return Err("upload_reaper.stale_after_secs must be in 60..=86400".to_owned());
        }
        let ts = &self.thread_summary_worker;
        if !(30..=3600).contains(&ts.claim_timeout_secs) {
            return Err("thread_summary_worker.claim_timeout_secs must be in 30..=3600".to_owned());
        }
        if ts.max_attempts == 0 {
            return Err("thread_summary_worker.max_attempts must be > 0".to_owned());
        }
        if !(1..=99).contains(&ts.compression_threshold_pct) {
            return Err("thread_summary_worker.compression_threshold_pct must be in 1..=99".to_owned());
        }
        if self.cleanup_worker.max_attempts == 0 {
            return Err("cleanup_worker.max_attempts must be > 0".to_owned());
        }
        self.validate_providers()
    }

    fn validate_providers(&self) -> Result<(), String> {
        for (id, p) in &self.providers {
            if !is_valid_host(p.host.trim()) {
                return Err(format!("providers.{id}.host is empty or contains invalid characters"));
            }
            if p.port == Some(0) {
                return Err(format!("providers.{id}.port must not be 0"));
            }
            if !p.api_path.starts_with('/') {
                return Err(format!("providers.{id}.api_path must start with '/'"));
            }
            if p.storage_kind == StorageKind::Azure {
                let v = p.api_version.as_deref().unwrap_or("").trim();
                if v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
                    return Err(format!(
                        "providers.{id}.api_version is required for storage_kind azure (letters, digits, '.', '-')"
                    ));
                }
            }
            if let Some(rag) = &p.rag_provider
                && !self.providers.contains_key(rag)
            {
                return Err(format!("providers.{id}.rag_provider '{rag}' is not a configured provider"));
            }
            for (tid, o) in &p.tenant_overrides {
                if uuid::Uuid::parse_str(tid).is_err() {
                    return Err(format!("providers.{id}.tenant_overrides key '{tid}' is not a UUID"));
                }
                if o.host.is_none() && o.upstream_alias.is_none() {
                    return Err(format!(
                        "providers.{id}.tenant_overrides.{tid} must set host or upstream_alias"
                    ));
                }
                if let Some(h) = &o.host
                    && !is_valid_host(h.trim())
                {
                    return Err(format!("providers.{id}.tenant_overrides.{tid}.host is invalid"));
                }
            }
        }
        if let Some(pid) = self.knowledge_search.provider_id.as_ref().filter(|_| self.knowledge_search.enabled)
            && !self.providers.contains_key(pid)
        {
            return Err(format!("knowledge_search.provider_id '{pid}' is not a configured provider"));
        }
        Ok(())
    }

    /// Fills `upstream_alias` of every entry and override (DESIGN "OAGW provisioning").
    pub fn fill_aliases(&mut self) {
        for p in self.providers.values_mut() {
            let port = p.effective_port();
            if p.upstream_alias.as_deref().is_none_or(|a| a.trim().is_empty()) {
                p.upstream_alias = Some(derive_alias(&p.host, port));
            }
            for o in p.tenant_overrides.values_mut() {
                if o.upstream_alias.as_deref().is_none_or(|a| a.trim().is_empty())
                    && let Some(h) = &o.host
                {
                    o.upstream_alias = Some(derive_alias(h, port));
                }
            }
        }
    }

    /// Warnings for deprecated fields set to non-default values (ADR-0010).
    #[must_use]
    pub fn deprecation_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let d = GearEstimationBudgets::default();
        let e = &self.estimation_budgets;
        for (name, v, dv) in [
            ("bytes_per_token_conservative", e.bytes_per_token_conservative, d.bytes_per_token_conservative),
            ("fixed_overhead_tokens", e.fixed_overhead_tokens, d.fixed_overhead_tokens),
            ("safety_margin_pct", e.safety_margin_pct, d.safety_margin_pct),
            ("image_token_budget", e.image_token_budget, d.image_token_budget),
            ("tool_surcharge_tokens", e.tool_surcharge_tokens, d.tool_surcharge_tokens),
            ("web_search_surcharge_tokens", e.web_search_surcharge_tokens, d.web_search_surcharge_tokens),
            (
                "code_interpreter_surcharge_tokens",
                e.code_interpreter_surcharge_tokens,
                d.code_interpreter_surcharge_tokens,
            ),
        ] {
            if v != dv {
                out.push(format!("estimation_budgets.{name} is deprecated and has no effect"));
            }
        }
        let c = &self.cleanup_worker;
        let cd = CleanupWorkerConfig::default();
        if !c.enabled {
            out.push("cleanup_worker.enabled is deprecated and has no effect".to_owned());
        }
        if c.poll_interval_secs != cd.poll_interval_secs {
            out.push("cleanup_worker.poll_interval_secs is deprecated and has no effect".to_owned());
        }
        if c.reconcile_interval_secs != cd.reconcile_interval_secs {
            out.push("cleanup_worker.reconcile_interval_secs is deprecated and has no effect".to_owned());
        }
        if c.stale_in_progress_timeout_secs != cd.stale_in_progress_timeout_secs {
            out.push(
                "cleanup_worker.stale_in_progress_timeout_secs is deprecated and has no effect".to_owned(),
            );
        }
        if c.batch_size != cd.batch_size {
            out.push("cleanup_worker.batch_size is deprecated and has no effect".to_owned());
        }
        if self.thread_summary_worker.reconcile_interval_secs
            != ThreadSummaryWorkerConfig::default().reconcile_interval_secs
        {
            out.push(
                "thread_summary_worker.reconcile_interval_secs is deprecated and has no effect".to_owned(),
            );
        }
        out
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
