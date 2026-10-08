//! Gear configuration (`gears.mini-chat.config`), see DESIGN Appendix B.1 and B.4-B.9.
//!
//! Every section has a full [`Default`]. Top level and the sections listed in B.1 reject
//! unknown keys; the worker sections (`orphan_watchdog`, `upload_reaper`,
//! `thread_summary_worker`, `cleanup_worker`) accept and ignore them.
//!
//! Load with `ctx.config_expanded_or_default::<MiniChatConfig>()` (expands `${VAR}` in provider
//! `host` / `auth_config`, the same fields of tenant overrides, and `client_credentials`), then
//! call [`MiniChatConfig::apply_defaults`] and [`MiniChatConfig::validate`].

use std::collections::HashMap;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use uuid::Uuid;

/// Default `context.web_search_guard` (DESIGN §4 "Web Search Configuration").
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Default `context.file_search_guard` (spec §9 decision).
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search to look up information in the documents attached to this chat when the question relates to them. Do not use it for general knowledge questions.";

/// Default `knowledge_search.guard` (DESIGN only says "built-in text").
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to look up information in the organization knowledge base when the question relates to it. Do not use it for general knowledge questions.";

/// Default `thread_summary_worker.summary_system_prompt` (DESIGN B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Model used for summaries when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// OAGW API-key auth plugin GTS id used by the default provider.
const APIKEY_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// Id of the provider entry present in the default configuration.
pub const DEFAULT_PROVIDER_ID: &str = "openai";

// ---------------------------------------------------------------------------------------------
// Top level
// ---------------------------------------------------------------------------------------------

/// Root of `gears.mini-chat.config`.
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
    /// Provider registry keyed by provider id.
    #[expand_vars]
    pub providers: HashMap<String, ProviderEntry>,
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
            client_credentials: ClientCredentialsConfig::default(),
            metrics: MetricsConfig::default(),
            providers: HashMap::from([(DEFAULT_PROVIDER_ID.to_owned(), ProviderEntry::openai())]),
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

impl MiniChatConfig {
    /// Fills values derived from other keys: provider `upstream_alias` (entry host; override
    /// host), `storage_backend` (provider id) and `port` (443, or 80 with `use_http`).
    pub fn apply_defaults(&mut self) {
        for (id, entry) in &mut self.providers {
            if entry.upstream_alias.is_none() {
                entry.upstream_alias = Some(entry.host.clone());
            }
            if entry.storage_backend.is_none() {
                entry.storage_backend = Some(id.clone());
            }
            if entry.port.is_none() {
                entry.port = Some(if entry.use_http { 80 } else { 443 });
            }
            for ov in entry.tenant_overrides.values_mut() {
                if ov.upstream_alias.is_none() {
                    ov.upstream_alias.clone_from(&ov.host);
                }
            }
        }
    }

    /// Validates every section (call after `${VAR}` expansion and [`Self::apply_defaults`]).
    ///
    /// # Errors
    /// Returns a message naming the first offending key.
    pub fn validate(&self) -> Result<(), String> {
        non_empty("vendor", &self.vendor)?;
        self.client_credentials.validate()?;
        let mut ids: Vec<&String> = self.providers.keys().collect();
        ids.sort();
        for id in ids {
            self.providers[id].validate(id, &self.providers)?;
        }
        self.streaming.validate()?;
        self.estimation_budgets
            .validate(self.streaming.max_output_tokens)?;
        self.quota.validate()?;
        self.outbox.validate()?;
        self.context.validate()?;
        self.rag.validate()?;
        self.thumbnail.validate()?;
        self.knowledge_search.validate()?;
        self.orphan_watchdog.validate()?;
        self.upload_reaper.validate()?;
        self.thread_summary_worker.validate()?;
        self.cleanup_worker.validate()?;
        Ok(())
    }

    /// One warning per deprecated key set to a non-default value (logged at gear init).
    #[must_use]
    pub fn deprecation_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        let eb = &self.estimation_budgets;
        let d = EstimationBudgetsConfig::default();
        let mut est = |name: &str, set: u32, default: u32| {
            if set != default {
                out.push(format!(
                    "estimation_budgets.{name} is deprecated and has no effect"
                ));
            }
        };
        est(
            "bytes_per_token_conservative",
            eb.bytes_per_token_conservative,
            d.bytes_per_token_conservative,
        );
        est(
            "fixed_overhead_tokens",
            eb.fixed_overhead_tokens,
            d.fixed_overhead_tokens,
        );
        est(
            "safety_margin_pct",
            eb.safety_margin_pct,
            d.safety_margin_pct,
        );
        est(
            "image_token_budget",
            eb.image_token_budget,
            d.image_token_budget,
        );
        est(
            "tool_surcharge_tokens",
            eb.tool_surcharge_tokens,
            d.tool_surcharge_tokens,
        );
        est(
            "web_search_surcharge_tokens",
            eb.web_search_surcharge_tokens,
            d.web_search_surcharge_tokens,
        );
        est(
            "code_interpreter_surcharge_tokens",
            eb.code_interpreter_surcharge_tokens,
            d.code_interpreter_surcharge_tokens,
        );

        let cw = &self.cleanup_worker;
        let cd = CleanupWorkerConfig::default();
        if !cw.enabled {
            out.push("cleanup_worker.enabled is deprecated and has no effect".to_owned());
        }
        let mut cleanup = |name: &str, differs: bool| {
            if differs {
                out.push(format!(
                    "cleanup_worker.{name} is deprecated and has no effect"
                ));
            }
        };
        cleanup(
            "poll_interval_secs",
            cw.poll_interval_secs != cd.poll_interval_secs,
        );
        cleanup(
            "reconcile_interval_secs",
            cw.reconcile_interval_secs != cd.reconcile_interval_secs,
        );
        cleanup(
            "stale_in_progress_timeout_secs",
            cw.stale_in_progress_timeout_secs != cd.stale_in_progress_timeout_secs,
        );
        cleanup("batch_size", cw.batch_size != cd.batch_size);

        if self.thread_summary_worker.reconcile_interval_secs
            != ThreadSummaryWorkerConfig::default().reconcile_interval_secs
        {
            out.push(
                "thread_summary_worker.reconcile_interval_secs is deprecated and has no effect"
                    .to_owned(),
            );
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// client_credentials / metrics
// ---------------------------------------------------------------------------------------------

/// S2S credentials (required, non-empty; the secret is redacted in `Debug`).
#[derive(Debug, Clone, Default, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct ClientCredentialsConfig {
    #[expand_vars]
    pub client_id: String,
    #[expand_vars]
    pub client_secret: SecretString,
}

impl ClientCredentialsConfig {
    fn validate(&self) -> Result<(), String> {
        non_empty("client_credentials.client_id", &self.client_id)?;
        non_empty(
            "client_credentials.client_secret",
            self.client_secret.expose_secret(),
        )
    }
}

/// Metrics settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix; empty means `mini_chat`.
    pub prefix: String,
}

// ---------------------------------------------------------------------------------------------
// providers
// ---------------------------------------------------------------------------------------------

/// LLM provider adapter kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}

/// File / vector-store implementation of a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

fn default_api_path() -> String {
    "/v1/responses".to_owned()
}

/// One `providers.<id>` entry.
#[derive(Debug, Clone, Deserialize, toolkit_macros::ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    /// OAGW upstream host; the character set is checked after `${VAR}` expansion.
    #[expand_vars]
    pub host: String,
    /// `None` until [`MiniChatConfig::apply_defaults`] (443, or 80 with `use_http`).
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub use_http: bool,
    /// `None` until [`MiniChatConfig::apply_defaults`] (the host).
    #[serde(default)]
    pub upstream_alias: Option<String>,
    #[serde(default = "default_api_path")]
    pub api_path: String,
    #[serde(default)]
    pub auth_plugin_type: Option<String>,
    #[serde(default)]
    #[expand_vars]
    pub auth_config: HashMap<String, String>,
    pub storage_kind: StorageKind,
    /// `None` until [`MiniChatConfig::apply_defaults`] (the provider id).
    #[serde(default)]
    pub storage_backend: Option<String>,
    /// Required (non-blank) when `storage_kind = azure`.
    #[serde(default)]
    pub api_version: Option<String>,
    /// Provider used for file / vector-store operations.
    #[serde(default)]
    pub rag_provider: Option<String>,
    #[serde(default)]
    #[expand_vars]
    pub tenant_overrides: HashMap<Uuid, TenantOverride>,
}

impl ProviderEntry {
    /// The built-in `openai` entry of the default configuration.
    fn openai() -> Self {
        Self {
            kind: ProviderKind::OpenaiResponses,
            host: "api.openai.com".to_owned(),
            port: None,
            use_http: false,
            upstream_alias: None,
            api_path: default_api_path(),
            auth_plugin_type: Some(APIKEY_AUTH_PLUGIN.to_owned()),
            auth_config: HashMap::from([
                ("header".to_owned(), "authorization".to_owned()),
                ("prefix".to_owned(), "Bearer ".to_owned()),
                ("secret_ref".to_owned(), "cred://openai-key".to_owned()),
            ]),
            storage_kind: StorageKind::Openai,
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: HashMap::new(),
        }
    }

    /// Provider id used for file / vector-store operations of this entry.
    #[must_use]
    pub fn rag_provider_id<'a>(&'a self, own_id: &'a str) -> &'a str {
        self.rag_provider.as_deref().unwrap_or(own_id)
    }

    /// Effective port (explicit, else 443 / 80 with `use_http`).
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }

    /// Effective upstream alias (explicit, else the host).
    #[must_use]
    pub fn effective_upstream_alias(&self) -> &str {
        self.upstream_alias.as_deref().unwrap_or(&self.host)
    }

    fn validate(&self, id: &str, all: &HashMap<String, ProviderEntry>) -> Result<(), String> {
        let p = format!("providers.{id}");
        check_host(&format!("{p}.host"), &self.host)?;
        if self.port == Some(0) {
            return Err(format!("{p}.port must not be 0"));
        }
        if self.storage_kind == StorageKind::Azure
            && self
                .api_version
                .as_deref()
                .is_none_or(|v| v.trim().is_empty())
        {
            return Err(format!(
                "{p}.api_version is required when storage_kind = azure"
            ));
        }
        if let Some(v) = self.api_version.as_deref().filter(|v| !v.trim().is_empty())
            && !v
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return Err(format!(
                "{p}.api_version may contain only letters, digits, '.' and '-'"
            ));
        }
        if let Some(r) = &self.rag_provider
            && !all.contains_key(r)
        {
            return Err(format!(
                "{p}.rag_provider '{r}' is not a configured provider"
            ));
        }
        for (tid, ov) in &self.tenant_overrides {
            ov.validate(&format!("{p}.tenant_overrides.{tid}"))?;
        }
        Ok(())
    }
}

/// `providers.<id>.tenant_overrides.<tenant_id>`; each unset field falls back to the entry.
#[derive(Debug, Clone, Default, Deserialize, toolkit_macros::ExpandVars)]
#[serde(default, deny_unknown_fields)]
pub struct TenantOverride {
    #[expand_vars]
    pub host: Option<String>,
    /// Filled with the override host by [`MiniChatConfig::apply_defaults`] when unset.
    pub upstream_alias: Option<String>,
    pub auth_plugin_type: Option<String>,
    #[expand_vars]
    pub auth_config: Option<HashMap<String, String>>,
}

impl TenantOverride {
    fn validate(&self, p: &str) -> Result<(), String> {
        if self.host.is_none() && self.upstream_alias.is_none() {
            return Err(format!("{p} must set host or upstream_alias"));
        }
        if let Some(h) = &self.host {
            check_host(&format!("{p}.host"), h)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// streaming / estimation / quota / outbox / context / rag / thumbnail / knowledge
// ---------------------------------------------------------------------------------------------

/// `streaming` section (B.4).
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
            max_output_tokens: 32768,
        }
    }
}

impl StreamingConfig {
    fn validate(&self) -> Result<(), String> {
        in_range(
            "streaming.sse_ping_interval_seconds",
            self.sse_ping_interval_seconds,
            5,
            60,
        )?;
        in_range(
            "streaming.sse_channel_capacity",
            self.sse_channel_capacity,
            16,
            64,
        )
    }
}

/// `estimation_budgets` section (B.5.2); only `minimal_generation_floor` is used.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EstimationBudgetsConfig {
    pub minimal_generation_floor: u32,
    // Deprecated: parsed, not validated, warn when non-default.
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

impl EstimationBudgetsConfig {
    fn validate(&self, max_output_tokens: u32) -> Result<(), String> {
        in_range(
            "estimation_budgets.minimal_generation_floor",
            self.minimal_generation_floor,
            1,
            max_output_tokens,
        )
    }
}

/// `quota` section (B.5.3, B.6, B.6.1).
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

impl QuotaConfig {
    fn validate(&self) -> Result<(), String> {
        in_range(
            "quota.overshoot_tolerance_factor",
            self.overshoot_tolerance_factor,
            1.0,
            1.5,
        )?;
        in_range(
            "quota.warning_threshold_pct",
            self.warning_threshold_pct,
            1,
            99,
        )?;
        positive(
            "quota.web_search_max_calls_per_message",
            self.web_search_max_calls_per_message,
        )?;
        positive("quota.web_search_daily_quota", self.web_search_daily_quota)?;
        positive(
            "quota.code_interpreter_max_calls_per_message",
            self.code_interpreter_max_calls_per_message,
        )?;
        positive(
            "quota.code_interpreter_daily_quota",
            self.code_interpreter_daily_quota,
        )
    }
}

/// `outbox` section (B.9.3).
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

impl OutboxConfig {
    fn validate(&self) -> Result<(), String> {
        non_empty("outbox.queue_name", &self.queue_name)?;
        non_empty("outbox.cleanup_queue_name", &self.cleanup_queue_name)?;
        non_empty(
            "outbox.chat_cleanup_queue_name",
            &self.chat_cleanup_queue_name,
        )?;
        non_empty(
            "outbox.thread_summary_queue_name",
            &self.thread_summary_queue_name,
        )?;
        non_empty("outbox.audit_queue_name", &self.audit_queue_name)?;
        in_range("outbox.num_partitions", self.num_partitions, 1, 64)?;
        if !self.num_partitions.is_power_of_two() {
            return Err(format!(
                "outbox.num_partitions must be a power of 2, got {}",
                self.num_partitions
            ));
        }
        Ok(())
    }
}

/// `context` section (B.5.1).
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

impl ContextConfig {
    fn validate(&self) -> Result<(), String> {
        in_range(
            "context.recent_messages_limit",
            self.recent_messages_limit,
            0,
            100,
        )
    }
}

/// `rag` section (B.7, B.8).
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
            uploaded_file_max_size_kb: 25600,
            uploaded_image_max_size_kb: 5120,
            max_images_per_message: 4,
        }
    }
}

impl RagConfig {
    fn validate(&self) -> Result<(), String> {
        positive("rag.max_documents_per_chat", self.max_documents_per_chat)?;
        positive(
            "rag.max_total_upload_mb_per_chat",
            self.max_total_upload_mb_per_chat,
        )?;
        in_range(
            "rag.max_concurrent_uploads",
            self.max_concurrent_uploads,
            1,
            256,
        )?;
        positive(
            "rag.uploaded_file_max_size_kb",
            self.uploaded_file_max_size_kb,
        )?;
        positive(
            "rag.uploaded_image_max_size_kb",
            self.uploaded_image_max_size_kb,
        )?;
        positive("rag.max_images_per_message", self.max_images_per_message)
    }
}

/// `thumbnail` section (B.8).
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

impl ThumbnailConfig {
    fn validate(&self) -> Result<(), String> {
        positive("thumbnail.width", self.width)?;
        positive("thumbnail.height", self.height)?;
        positive("thumbnail.max_bytes", self.max_bytes)?;
        positive("thumbnail.max_pixels", self.max_pixels)?;
        positive("thumbnail.max_decode_bytes", self.max_decode_bytes)
    }
}

/// `knowledge_search` section (B.7).
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

impl KnowledgeSearchConfig {
    fn validate(&self) -> Result<(), String> {
        if self.enabled {
            for (key, v) in [
                ("knowledge_search.vector_store_id", &self.vector_store_id),
                ("knowledge_search.provider_id", &self.provider_id),
            ] {
                if v.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    return Err(format!("{key} is required when knowledge_search.enabled"));
                }
            }
        }
        positive(
            "knowledge_search.max_calls_per_message",
            self.max_calls_per_message,
        )?;
        positive("knowledge_search.top_k", self.top_k)?;
        positive("knowledge_search.max_chunk_chars", self.max_chunk_chars)
    }
}

// ---------------------------------------------------------------------------------------------
// worker sections (unknown keys are accepted and ignored)
// ---------------------------------------------------------------------------------------------

/// `orphan_watchdog` section (B.9.1).
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

impl OrphanWatchdogConfig {
    fn validate(&self) -> Result<(), String> {
        in_range("orphan_watchdog.timeout_secs", self.timeout_secs, 90, 3600)?;
        in_range(
            "orphan_watchdog.scan_interval_secs",
            self.scan_interval_secs,
            1,
            3600,
        )
    }
}

/// `upload_reaper` section (B.9.5).
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

impl UploadReaperConfig {
    fn validate(&self) -> Result<(), String> {
        in_range(
            "upload_reaper.scan_interval_secs",
            self.scan_interval_secs,
            1,
            3600,
        )?;
        in_range(
            "upload_reaper.stale_after_secs",
            self.stale_after_secs,
            60,
            86400,
        )
    }
}

/// `thread_summary_worker` section (B.9.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThreadSummaryWorkerConfig {
    pub enabled: bool,
    pub claim_timeout_secs: u64,
    pub max_attempts: u32,
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
    /// Catalog model used for summaries (`""` means `gpt-4.1-mini`).
    #[must_use]
    pub fn effective_model_id(&self) -> &str {
        if self.summary_model_id.is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            &self.summary_model_id
        }
    }

    fn validate(&self) -> Result<(), String> {
        in_range(
            "thread_summary_worker.claim_timeout_secs",
            self.claim_timeout_secs,
            30,
            3600,
        )?;
        positive("thread_summary_worker.max_attempts", self.max_attempts)?;
        in_range(
            "thread_summary_worker.compression_threshold_pct",
            self.compression_threshold_pct,
            1,
            99,
        )
    }
}

/// `cleanup_worker` section (B.9.2); everything except `max_attempts` is deprecated.
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

impl CleanupWorkerConfig {
    fn validate(&self) -> Result<(), String> {
        positive("cleanup_worker.max_attempts", self.max_attempts)
    }
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn non_empty(key: &str, v: &str) -> Result<(), String> {
    if v.trim().is_empty() {
        Err(format!("{key} must be non-empty"))
    } else {
        Ok(())
    }
}

fn positive<T: PartialOrd + Default + std::fmt::Display + Copy>(
    key: &str,
    v: T,
) -> Result<(), String> {
    if v > T::default() {
        Ok(())
    } else {
        Err(format!("{key} must be > 0, got {v}"))
    }
}

fn in_range<T: PartialOrd + std::fmt::Display + Copy>(
    key: &str,
    v: T,
    lo: T,
    hi: T,
) -> Result<(), String> {
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(format!("{key} must be in {lo}..={hi}, got {v}"))
    }
}

/// Host is the OAGW alias in `/{alias}/...`: only `[A-Za-z0-9._:\[\]-]` is allowed.
fn check_host(key: &str, host: &str) -> Result<(), String> {
    non_empty(key, host)?;
    if host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '[' | ']' | '-'))
    {
        Ok(())
    } else {
        Err(format!(
            "{key} '{host}' may contain only letters, digits and . - _ : [ ]"
        ))
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
