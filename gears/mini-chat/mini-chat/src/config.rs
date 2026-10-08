//! Gear configuration (`gears.mini-chat.config`).
//!
//! Sections, defaults and validation follow DESIGN Appendix B.1-B.9. Unknown keys
//! are rejected at the top level and in every section except the worker sections
//! (`orphan_watchdog`, `upload_reaper`, `thread_summary_worker`, `cleanup_worker`),
//! which accept and ignore them.
//!
//! Load with `ctx.config_expanded_or_default::<MiniChatConfig>()`, then call
//! [`MiniChatConfig::fill_upstream_aliases`] and [`MiniChatConfig::validate`].

use std::collections::HashMap;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

/// OAGW API-key auth plugin GTS id used by the default provider entry.
const APIKEY_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// Model used for thread summaries when `summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Built-in `context.web_search_guard` (DESIGN section 4, web search).
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Built-in `context.file_search_guard`.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search only when the answer depends on the documents the user uploaded to this chat. Do not use it for general knowledge questions or when the provided context already answers the question. Base your answer on the returned excerpts and say so when they do not contain the answer.";

/// Built-in `knowledge_search.guard`.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge only when the answer depends on information from the shared knowledge base. Do not use it for general knowledge questions or when the provided context already answers the question. Base your answer on the returned excerpts and say so when they do not contain the answer.";

/// Built-in summary system prompt (DESIGN B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

// ── Top level ────────────────────────────────────────────────────────────────

/// Root of the gear configuration.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    /// Prefix of all REST routes.
    pub url_prefix: String,
    /// Selects the model-policy and audit plugin instances.
    pub vendor: String,
    /// S2S credentials exchanged via `authn_resolver`.
    #[expand_vars]
    pub client_credentials: ClientCredentialsConfig,
    pub metrics: MetricsConfig,
    /// Provider registry; key = provider id.
    #[expand_vars]
    pub providers: HashMap<String, ProviderEntry>,
    pub streaming: StreamingConfig,
    pub estimation_budgets: GearEstimationBudgets,
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
            client_credentials: ClientCredentialsConfig::default(),
            metrics: MetricsConfig::default(),
            providers: HashMap::from([("openai".to_owned(), ProviderEntry::default_openai())]),
            streaming: StreamingConfig::default(),
            estimation_budgets: GearEstimationBudgets::default(),
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

/// S2S client credentials (required, non-empty). The secret is redacted in `Debug`.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix; empty means `mini_chat`.
    pub prefix: String,
}

// ── Providers ────────────────────────────────────────────────────────────────

/// Provider adapter kind (ADR-0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

/// File / vector-store implementation selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

/// One entry of `providers.<id>`.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    #[expand_vars]
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub use_http: bool,
    /// Filled from `host` by [`MiniChatConfig::fill_upstream_aliases`] when unset.
    #[serde(default)]
    pub upstream_alias: Option<String>,
    #[serde(default = "default_api_path")]
    pub api_path: String,
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    #[serde(default)]
    #[expand_vars]
    pub auth_config: HashMap<String, String>,
    #[serde(default)]
    pub storage_kind: Option<StorageKind>,
    #[serde(default)]
    pub storage_backend: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub rag_provider: Option<String>,
    /// Per-tenant overrides; key = tenant id.
    #[serde(default)]
    #[expand_vars]
    pub tenant_overrides: HashMap<String, TenantOverride>,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

impl ProviderEntry {
    fn default_openai() -> Self {
        Self {
            kind: ProviderKind::OpenaiResponses,
            host: "api.openai.com".to_owned(),
            port: None,
            use_http: false,
            upstream_alias: None,
            api_path: default_api_path(),
            auth_plugin_type: Some(APIKEY_AUTH_PLUGIN.to_owned()),
            auth_config: HashMap::from([
                ("header".to_owned(), "Authorization".to_owned()),
                ("prefix".to_owned(), "Bearer ".to_owned()),
                ("secret_ref".to_owned(), "cred://openai-key".to_owned()),
            ]),
            storage_kind: Some(StorageKind::Openai),
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: HashMap::new(),
        }
    }

    /// OAGW upstream alias: `upstream_alias` (filled at init), else the host.
    #[must_use]
    pub fn alias(&self) -> &str {
        self.upstream_alias.as_deref().unwrap_or(&self.host)
    }

    /// Configured port, else 443 (80 when `use_http`).
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }

    /// Label stored in `attachments.storage_backend` / `chat_vector_stores.provider`:
    /// the configured `storage_backend`, else the provider id.
    #[must_use]
    pub fn storage_backend_label(&self, id: &str) -> String {
        self.storage_backend
            .clone()
            .unwrap_or_else(|| id.to_owned())
    }
}

/// `providers.<id>.tenant_overrides.<tenant_id>`; unset fields inherit from the provider.
#[derive(Debug, Clone, Default, Deserialize, toolkit_macros::ExpandVars)]
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

// ── Streaming / budgets / quota ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// `5..=60`.
    pub sse_ping_interval_seconds: u16,
    /// `16..=64`.
    pub sse_channel_capacity: u16,
    /// Cap on the applied `max_output_tokens`.
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

/// Gear-level estimation budgets. Only `minimal_generation_floor` is used; the other
/// fields are deprecated (parsed, not validated).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GearEstimationBudgets {
    pub bytes_per_token_conservative: u32,
    pub fixed_overhead_tokens: u32,
    pub safety_margin_pct: u32,
    pub image_token_budget: u32,
    pub tool_surcharge_tokens: u32,
    pub web_search_surcharge_tokens: u32,
    pub code_interpreter_surcharge_tokens: u32,
    /// `> 0` and `<= streaming.max_output_tokens`.
    pub minimal_generation_floor: u32,
}

impl Default for GearEstimationBudgets {
    fn default() -> Self {
        Self {
            bytes_per_token_conservative: 4,
            fixed_overhead_tokens: 100,
            safety_margin_pct: 10,
            image_token_budget: 1000,
            tool_surcharge_tokens: 500,
            web_search_surcharge_tokens: 500,
            code_interpreter_surcharge_tokens: 1000,
            minimal_generation_floor: 50,
        }
    }
}

impl GearEstimationBudgets {
    /// One warning per deprecated field set to a non-default value.
    #[must_use]
    pub fn deprecated_warnings(&self) -> Vec<String> {
        let d = Self::default();
        let fields = [
            (
                "bytes_per_token_conservative",
                self.bytes_per_token_conservative,
                d.bytes_per_token_conservative,
            ),
            (
                "fixed_overhead_tokens",
                self.fixed_overhead_tokens,
                d.fixed_overhead_tokens,
            ),
            (
                "safety_margin_pct",
                self.safety_margin_pct,
                d.safety_margin_pct,
            ),
            (
                "image_token_budget",
                self.image_token_budget,
                d.image_token_budget,
            ),
            (
                "tool_surcharge_tokens",
                self.tool_surcharge_tokens,
                d.tool_surcharge_tokens,
            ),
            (
                "web_search_surcharge_tokens",
                self.web_search_surcharge_tokens,
                d.web_search_surcharge_tokens,
            ),
            (
                "code_interpreter_surcharge_tokens",
                self.code_interpreter_surcharge_tokens,
                d.code_interpreter_surcharge_tokens,
            ),
        ];
        fields
            .into_iter()
            .filter(|(_, value, default)| value != default)
            .map(|(name, value, _)| {
                format!(
                    "estimation_budgets.{name} = {value} is deprecated and has no effect; \
                     token estimates use the model catalog estimation_budgets"
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    /// `1.0..=1.5`.
    pub overshoot_tolerance_factor: f64,
    /// `1..=99`.
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

// ── Outbox / context / rag / thumbnail ───────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutboxConfig {
    pub queue_name: String,
    pub cleanup_queue_name: String,
    pub chat_cleanup_queue_name: String,
    pub thread_summary_queue_name: String,
    pub audit_queue_name: String,
    /// Power of two in `1..=64`; shared by all five queues.
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
    /// `0..=100`.
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
    /// `1..=256`, per process.
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

// ── Workers (unknown keys accepted and ignored) ──────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrphanWatchdogConfig {
    pub enabled: bool,
    /// `1..=3600`.
    pub scan_interval_secs: u64,
    /// `90..=3600`.
    pub timeout_secs: u64,
}

impl Default for OrphanWatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_interval_secs: 60,
            timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UploadReaperConfig {
    pub enabled: bool,
    /// `1..=3600`.
    pub scan_interval_secs: u64,
    /// `60..=86400`.
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
    /// `30..=3600`; outbox lease of the thread-summary queue.
    pub claim_timeout_secs: u64,
    pub max_attempts: u32,
    /// `1..=99`.
    pub compression_threshold_pct: u32,
    /// Empty means [`DEFAULT_SUMMARY_MODEL_ID`].
    pub summary_model_id: String,
    pub summary_system_prompt: String,
    /// Max characters per message in the prompt; 0 = no truncation.
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
    /// Configured summary model, or the built-in default when empty.
    #[must_use]
    pub fn effective_summary_model_id(&self) -> &str {
        if self.summary_model_id.is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            &self.summary_model_id
        }
    }
}

/// Only `max_attempts` is in use; the other fields are deprecated and have no effect.
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

// ── Knowledge search ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KnowledgeSearchConfig {
    pub enabled: bool,
    /// Required when enabled.
    pub vector_store_id: Option<String>,
    /// Required when enabled.
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

// ── Fill + validation ────────────────────────────────────────────────────────

impl MiniChatConfig {
    /// One warning per deprecated field set to a non-default value:
    /// `estimation_budgets.*` (except `minimal_generation_floor`),
    /// `cleanup_worker.{enabled=false, poll_interval_secs, reconcile_interval_secs,
    /// stale_in_progress_timeout_secs, batch_size}` and
    /// `thread_summary_worker.reconcile_interval_secs`. Logged at startup.
    #[must_use]
    pub fn deprecated_warnings(&self) -> Vec<String> {
        let mut out = self.estimation_budgets.deprecated_warnings();
        let cw = &self.cleanup_worker;
        let cd = CleanupWorkerConfig::default();
        if !cw.enabled {
            out.push(
                "cleanup_worker.enabled = false is deprecated and has no effect; \
                 cleanup runs as outbox handlers"
                    .to_owned(),
            );
        }
        let numeric = [
            (
                "cleanup_worker.poll_interval_secs",
                cw.poll_interval_secs,
                cd.poll_interval_secs,
            ),
            (
                "cleanup_worker.reconcile_interval_secs",
                cw.reconcile_interval_secs,
                cd.reconcile_interval_secs,
            ),
            (
                "cleanup_worker.stale_in_progress_timeout_secs",
                cw.stale_in_progress_timeout_secs,
                cd.stale_in_progress_timeout_secs,
            ),
            (
                "cleanup_worker.batch_size",
                u64::from(cw.batch_size),
                u64::from(cd.batch_size),
            ),
            (
                "thread_summary_worker.reconcile_interval_secs",
                self.thread_summary_worker.reconcile_interval_secs,
                ThreadSummaryWorkerConfig::default().reconcile_interval_secs,
            ),
        ];
        out.extend(
            numeric
                .into_iter()
                .filter(|(_, value, default)| value != default)
                .map(|(name, value, _)| {
                    format!("{name} = {value} is deprecated and has no effect")
                }),
        );
        out
    }

    /// Fill `upstream_alias` where unset (for tenant overrides: from the override
    /// `host`, with the provider's port and scheme). The default is the host, except
    /// for a hostname on a non-standard port (80 for `use_http`, 443 otherwise),
    /// where it is `host:port` — the alias OAGW derives and the only one it accepts
    /// for such an endpoint. IP hosts keep the bare host (passed as explicit alias).
    /// Call after `${VAR}` expansion.
    pub fn fill_upstream_aliases(&mut self) {
        for entry in self.providers.values_mut() {
            let port = entry.effective_port();
            let use_http = entry.use_http;
            if entry.upstream_alias.is_none() {
                entry.upstream_alias = Some(default_alias(&entry.host, port, use_http));
            }
            for ov in entry.tenant_overrides.values_mut() {
                if ov.upstream_alias.is_none() {
                    ov.upstream_alias = ov
                        .host
                        .as_deref()
                        .map(|host| default_alias(host, port, use_http));
                }
            }
        }
    }

    /// Validate every rule of DESIGN B.1-B.9. Call after `${VAR}` expansion.
    ///
    /// # Errors
    /// Returns a description of the first invalid field.
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.trim().is_empty() {
            return Err("vendor must be non-empty".to_owned());
        }
        if !self.url_prefix.starts_with('/') || self.url_prefix.ends_with('/') {
            return Err(format!(
                "url_prefix must start with '/' and must not end with '/', got '{}'",
                self.url_prefix
            ));
        }
        if self.client_credentials.client_id.trim().is_empty() {
            return Err("client_credentials.client_id must be non-empty".to_owned());
        }
        if self
            .client_credentials
            .client_secret
            .expose_secret()
            .trim()
            .is_empty()
        {
            return Err("client_credentials.client_secret must be non-empty".to_owned());
        }
        self.validate_providers()?;
        self.validate_streaming_and_quota()?;
        self.validate_outbox_context_rag()?;
        self.validate_workers()?;
        self.validate_knowledge_search()
    }

    fn validate_providers(&self) -> Result<(), String> {
        for (id, entry) in &self.providers {
            let at = format!("providers.{id}");
            check_host(&format!("{at}.host"), &entry.host)?;
            if entry.port == Some(0) {
                return Err(format!("{at}.port must not be 0"));
            }
            if let Some(target) = &entry.rag_provider
                && !self.providers.contains_key(target)
            {
                return Err(format!(
                    "{at}.rag_provider '{target}' does not name an existing provider"
                ));
            }
            let storage_optional =
                entry.kind == ProviderKind::AnthropicMessages && entry.rag_provider.is_some();
            if entry.storage_kind.is_none() && !storage_optional {
                return Err(format!("{at}.storage_kind is required"));
            }
            if entry.storage_kind == Some(StorageKind::Azure) {
                match entry.api_version.as_deref() {
                    None => {
                        return Err(format!(
                            "{at}.api_version is required when storage_kind = azure"
                        ));
                    }
                    Some(v) if v.trim().is_empty() => {
                        return Err(format!("{at}.api_version must not be blank"));
                    }
                    Some(v)
                        if !v
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') =>
                    {
                        return Err(format!(
                            "{at}.api_version may contain only letters, digits, '.' and '-'"
                        ));
                    }
                    Some(_) => {}
                }
            }
            for (tenant, ov) in &entry.tenant_overrides {
                let ov_at = format!("{at}.tenant_overrides.{tenant}");
                if ov.host.is_none() && ov.upstream_alias.is_none() {
                    return Err(format!("{ov_at} must set host or upstream_alias"));
                }
                if let Some(host) = &ov.host {
                    check_host(&format!("{ov_at}.host"), host)?;
                }
            }
        }
        Ok(())
    }

    fn validate_streaming_and_quota(&self) -> Result<(), String> {
        let s = &self.streaming;
        check_range(
            "streaming.sse_ping_interval_seconds",
            s.sse_ping_interval_seconds,
            5..=60,
        )?;
        check_range(
            "streaming.sse_channel_capacity",
            s.sse_channel_capacity,
            16..=64,
        )?;
        let floor = self.estimation_budgets.minimal_generation_floor;
        if floor == 0 || floor > s.max_output_tokens {
            return Err(format!(
                "estimation_budgets.minimal_generation_floor ({floor}) must be > 0 and <= \
                 streaming.max_output_tokens ({})",
                s.max_output_tokens
            ));
        }
        let q = &self.quota;
        if !(1.0..=1.5).contains(&q.overshoot_tolerance_factor) {
            return Err(format!(
                "quota.overshoot_tolerance_factor must be within 1.0..=1.5, got {}",
                q.overshoot_tolerance_factor
            ));
        }
        check_range(
            "quota.warning_threshold_pct",
            q.warning_threshold_pct,
            1..=99,
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
        )
    }

    fn validate_outbox_context_rag(&self) -> Result<(), String> {
        let o = &self.outbox;
        for (name, value) in [
            ("queue_name", &o.queue_name),
            ("cleanup_queue_name", &o.cleanup_queue_name),
            ("chat_cleanup_queue_name", &o.chat_cleanup_queue_name),
            ("thread_summary_queue_name", &o.thread_summary_queue_name),
            ("audit_queue_name", &o.audit_queue_name),
        ] {
            if value.trim().is_empty() {
                return Err(format!("outbox.{name} must be non-empty"));
            }
        }
        if !(1..=64).contains(&o.num_partitions) || !o.num_partitions.is_power_of_two() {
            return Err(format!(
                "outbox.num_partitions must be a power of 2 in 1..=64, got {}",
                o.num_partitions
            ));
        }
        check_range(
            "context.recent_messages_limit",
            self.context.recent_messages_limit,
            0..=100,
        )?;
        let r = &self.rag;
        check_positive("rag.max_documents_per_chat", r.max_documents_per_chat)?;
        check_positive(
            "rag.max_total_upload_mb_per_chat",
            r.max_total_upload_mb_per_chat,
        )?;
        check_range(
            "rag.max_concurrent_uploads",
            r.max_concurrent_uploads,
            1..=256,
        )?;
        check_positive("rag.uploaded_file_max_size_kb", r.uploaded_file_max_size_kb)?;
        check_positive(
            "rag.uploaded_image_max_size_kb",
            r.uploaded_image_max_size_kb,
        )?;
        check_positive("rag.max_images_per_message", r.max_images_per_message)?;
        let t = &self.thumbnail;
        check_positive("thumbnail.width", t.width)?;
        check_positive("thumbnail.height", t.height)?;
        check_positive("thumbnail.max_bytes", t.max_bytes)?;
        check_positive("thumbnail.max_pixels", t.max_pixels)?;
        check_positive("thumbnail.max_decode_bytes", t.max_decode_bytes)
    }

    fn validate_workers(&self) -> Result<(), String> {
        let w = &self.orphan_watchdog;
        check_range(
            "orphan_watchdog.scan_interval_secs",
            w.scan_interval_secs,
            1..=3600,
        )?;
        check_range("orphan_watchdog.timeout_secs", w.timeout_secs, 90..=3600)?;
        let u = &self.upload_reaper;
        check_range(
            "upload_reaper.scan_interval_secs",
            u.scan_interval_secs,
            1..=3600,
        )?;
        check_range(
            "upload_reaper.stale_after_secs",
            u.stale_after_secs,
            60..=86400,
        )?;
        let t = &self.thread_summary_worker;
        check_range(
            "thread_summary_worker.claim_timeout_secs",
            t.claim_timeout_secs,
            30..=3600,
        )?;
        check_positive("thread_summary_worker.max_attempts", t.max_attempts)?;
        check_range(
            "thread_summary_worker.compression_threshold_pct",
            t.compression_threshold_pct,
            1..=99,
        )?;
        check_positive(
            "cleanup_worker.max_attempts",
            self.cleanup_worker.max_attempts,
        )
    }

    fn validate_knowledge_search(&self) -> Result<(), String> {
        let k = &self.knowledge_search;
        if !k.enabled {
            return Ok(());
        }
        for (name, value) in [
            ("vector_store_id", &k.vector_store_id),
            ("provider_id", &k.provider_id),
        ] {
            if value.as_deref().is_none_or(|v| v.trim().is_empty()) {
                return Err(format!("knowledge_search.{name} is required when enabled"));
            }
        }
        check_positive(
            "knowledge_search.max_calls_per_message",
            k.max_calls_per_message,
        )?;
        check_positive("knowledge_search.top_k", k.top_k)?;
        check_positive("knowledge_search.max_chunk_chars", k.max_chunk_chars)
    }
}

/// Default OAGW alias for `host` (see [`MiniChatConfig::fill_upstream_aliases`]).
fn default_alias(host: &str, port: u16, use_http: bool) -> String {
    let is_ip = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok();
    let standard_port = if use_http { 80 } else { 443 };
    if is_ip || port == standard_port {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    }
}

/// The host becomes the OAGW alias in `/{alias}/...`, so `/`, `?`, `#`, `@` etc. are rejected.
fn check_host(field: &str, host: &str) -> Result<(), String> {
    if host.trim().is_empty() {
        return Err(format!("{field} must be non-empty"));
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err(format!(
            "{field} '{host}' may contain only letters, digits, '.', '-', '_', ':', '[' and ']'"
        ));
    }
    Ok(())
}

fn check_range<T>(field: &str, value: T, range: std::ops::RangeInclusive<T>) -> Result<(), String>
where
    T: PartialOrd + std::fmt::Display + Copy,
{
    if range.contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "{field} must be within {}..={}, got {value}",
            range.start(),
            range.end()
        ))
    }
}

fn check_positive<T>(field: &str, value: T) -> Result<(), String>
where
    T: PartialOrd + Default + Copy,
{
    if value > T::default() {
        Ok(())
    } else {
        Err(format!("{field} must be > 0"))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "config_tests.rs"]
mod config_tests;
