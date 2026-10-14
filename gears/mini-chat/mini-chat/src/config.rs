//! Gear configuration (`gears.mini-chat.config`).
//!
//! Keys, defaults and ranges are normative in DESIGN Appendix B (`B.1`, `B.4` to
//! `B.9`). The raw value is parsed into the typed structs below (unknown keys are rejected
//! except in the four worker sections), `${VAR}` is expanded in the fields that allow it, and
//! [`MiniChatConfig::validate`] checks every range and rule.

use std::collections::BTreeMap;
use std::fmt::Display;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use toolkit::var_expand::{ExpandVars, ExpandVarsError, expand_env_vars};

/// Startup error that names the offending key or section.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The raw value does not match the config schema (unknown or removed key, wrong type).
    #[error("invalid mini-chat configuration: {0}")]
    Parse(String),
    /// A value is outside its documented range or violates a rule.
    #[error("invalid mini-chat configuration key `{key}`: {reason}")]
    Invalid { key: String, reason: String },
}

fn invalid(key: impl Into<String>, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.into(),
        reason: reason.into(),
    }
}

// ── defaults ─────────────────────────────────────────────────────────────────

const DEFAULT_URL_PREFIX: &str = "/mini-chat";
const DEFAULT_VENDOR: &str = "constructorfabric";
const DEFAULT_API_PATH: &str = "/v1/responses";
const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained \
from the provided context or your training data. Never use it for general knowledge questions. \
At most one web_search call per request.";
const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search only when the answer depends on the \
content of the documents the user uploaded to this chat. Do not use it for general knowledge \
questions.";
const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge only if the answer cannot be \
obtained from the provided context; it searches the organization knowledge base.";
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a \
conversation (and optionally an existing summary), produce a detailed structured summary. \
Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final \
summary). Only the <summary> content will be stored. Do not invent information not present in \
the conversation.";
/// The OAGW API-key auth plugin of the default provider entry.
const APIKEY_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

fn default_url_prefix() -> String {
    DEFAULT_URL_PREFIX.to_owned()
}

fn default_vendor() -> String {
    DEFAULT_VENDOR.to_owned()
}

fn default_api_path() -> String {
    DEFAULT_API_PATH.to_owned()
}

/// One entry `openai` (`openai_responses`, `api.openai.com`, API-key auth from
/// `cred://openai-key`, `storage_kind = openai`) when no provider is configured.
fn default_providers() -> BTreeMap<String, ProviderConfig> {
    let auth_config = BTreeMap::from([
        ("header".to_owned(), "Authorization".to_owned()),
        ("prefix".to_owned(), "Bearer ".to_owned()),
        ("secret_ref".to_owned(), "cred://openai-key".to_owned()),
    ]);
    BTreeMap::from([(
        "openai".to_owned(),
        ProviderConfig {
            kind: ProviderKind::OpenaiResponses,
            host: "api.openai.com".to_owned(),
            port: None,
            use_http: false,
            upstream_alias: None,
            api_path: default_api_path(),
            auth_plugin_type: Some(APIKEY_AUTH_PLUGIN.to_owned()),
            auth_config,
            storage_kind: StorageKind::Openai,
            storage_backend: None,
            api_version: None,
            rag_provider: None,
            tenant_overrides: BTreeMap::new(),
        },
    )])
}

// ── sections ─────────────────────────────────────────────────────────────────

// @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-default
/// Adapter kind of a provider entry. The set is closed: any other `kind` fails the parse at
/// startup, so the adapter selection has no default arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiResponses,
    OpenaiChatCompletions,
    VllmResponses,
    AnthropicMessages,
}
// @cpt-end:cpt-cf-mini-chat-algo-message-streaming-adapter-kinds:p1:inst-kind-default

/// File and vector-store implementation of a provider entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Openai,
    Azure,
}

/// S2S credentials exchanged through `authn_resolver`. The secret is redacted in `Debug`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientCredentials {
    pub client_id: String,
    pub client_secret: SecretString,
}

/// `metrics` section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix; empty means `mini_chat`.
    pub prefix: String,
}

impl MetricsConfig {
    /// The prefix in effect: the configured one, or `mini_chat` when empty.
    #[must_use]
    pub fn effective_prefix(&self) -> String {
        if self.prefix.is_empty() {
            "mini_chat".to_owned()
        } else {
            self.prefix.clone()
        }
    }
}

/// `providers.<id>.tenant_overrides.<tenant_id>`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TenantOverride {
    pub host: Option<String>,
    pub upstream_alias: Option<String>,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<BTreeMap<String, String>>,
}

/// `providers.<id>` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
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
    pub storage_kind: StorageKind,
    #[serde(default)]
    pub storage_backend: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub rag_provider: Option<String>,
    #[serde(default)]
    pub tenant_overrides: BTreeMap<String, TenantOverride>,
}

impl ProviderConfig {
    /// The port in effect: the configured one, else `443` (`80` when `use_http`).
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or(if self.use_http { 80 } else { 443 })
    }
}

/// The alias OAGW derives for a hostname endpoint, `None` for an IP host (OAGW requires an
/// explicit alias there and accepts any). Mirrors `Endpoint::alias_contribution` and
/// `compute_derived_alias` of OAGW (`management/alias.rs`): the host is lowercased, stripped
/// of brackets and trailing dots, and `:port` is appended when the port is not the standard
/// one of the scheme (`80` for http, `443` for https). OAGW rejects any other explicit alias
/// on a hostname endpoint, so the configured alias must equal this value.
#[must_use]
pub fn oagw_derived_alias(host: &str, port: u16, use_http: bool) -> Option<String> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned();
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    let standard = if use_http { 80 } else { 443 };
    Some(if port == standard {
        bare
    } else {
        format!("{bare}:{port}")
    })
}

/// An explicit alias as OAGW stores it: lowercase, trailing dots stripped (`normalize_alias`).
#[must_use]
pub fn normalize_oagw_alias(alias: &str) -> String {
    alias.to_ascii_lowercase().trim_end_matches('.').to_owned()
}

/// The alias of an endpoint when none is configured: the OAGW-derived alias for a hostname
/// endpoint, else (IP host) the bare host.
///
/// DESIGN fills `upstream_alias` with the host; the `host:port` form for a non-standard port
/// is an OAGW constraint (it rejects any other alias on a hostname endpoint).
#[must_use]
pub fn default_upstream_alias(host: &str, port: u16, use_http: bool) -> String {
    oagw_derived_alias(host, port, use_http).unwrap_or_else(|| {
        host.strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
            .to_ascii_lowercase()
    })
}

/// Rejects an explicit alias that OAGW would refuse on a hostname endpoint.
fn check_alias_matches_derived(
    key: &str,
    alias: &str,
    host: &str,
    port: u16,
    use_http: bool,
) -> Result<(), ConfigError> {
    match oagw_derived_alias(host, port, use_http) {
        Some(derived) if normalize_oagw_alias(alias) != derived => Err(invalid(
            key,
            format!(
                "OAGW derives the alias of a hostname endpoint and accepts only that value \
                 (`{derived}`); remove the key or set it to `{derived}`"
            ),
        )),
        _ => Ok(()),
    }
}

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
            max_output_tokens: 32_768,
        }
    }
}

/// `estimation_budgets` section (B.5.2). Only `minimal_generation_floor` is used;
/// the other fields are deprecated: parsed, not validated, not used.
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

/// `rag` section (B.7, B.8).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RagConfig {
    pub max_documents_per_chat: u32,
    pub max_total_upload_mb_per_chat: u32,
    pub uploaded_file_max_size_kb: u32,
    pub uploaded_image_max_size_kb: u32,
    pub max_images_per_message: u32,
    pub allow_csv_upload: bool,
    pub max_concurrent_uploads: u16,
}

impl Default for RagConfig {
    fn default() -> Self {
        Self {
            max_documents_per_chat: 50,
            max_total_upload_mb_per_chat: 100,
            uploaded_file_max_size_kb: 25_600,
            uploaded_image_max_size_kb: 5120,
            max_images_per_message: 4,
            allow_csv_upload: true,
            max_concurrent_uploads: 10,
        }
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

/// `knowledge_search` section (B.7).
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

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-ignore-worker-unknown
// The four worker sections below carry no `deny_unknown_fields`: unknown keys are accepted
// and ignored.

/// `orphan_watchdog` section (B.9.1). Unknown keys are accepted and ignored.
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

/// `upload_reaper` section (B.9.5). Unknown keys are accepted and ignored.
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

/// `thread_summary_worker` section (B.9.4). Unknown keys are accepted and ignored.
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

/// `cleanup_worker` section (B.9.2). Unknown keys are accepted and ignored.
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
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-ignore-worker-unknown

/// The typed gear configuration with all defaults applied.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiniChatConfig {
    #[serde(default = "default_url_prefix")]
    pub url_prefix: String,
    #[serde(default = "default_vendor")]
    pub vendor: String,
    pub client_credentials: ClientCredentials,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default = "default_providers")]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub estimation_budgets: EstimationBudgetsConfig,
    #[serde(default)]
    pub quota: QuotaConfig,
    #[serde(default)]
    pub outbox: OutboxConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub rag: RagConfig,
    #[serde(default)]
    pub thumbnail: ThumbnailConfig,
    #[serde(default)]
    pub knowledge_search: KnowledgeSearchConfig,
    #[serde(default)]
    pub orphan_watchdog: OrphanWatchdogConfig,
    #[serde(default)]
    pub upload_reaper: UploadReaperConfig,
    #[serde(default)]
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    #[serde(default)]
    pub cleanup_worker: CleanupWorkerConfig,
}

impl MiniChatConfig {
    /// Typed parse of the raw `gears.mini-chat.config` value: shape, defaults and the
    /// unknown-key rules, without range validation and without `${VAR}` expansion.
    ///
    /// Rejects unknown keys at the top level and inside `streaming`,
    /// `estimation_budgets`, `quota`, `outbox`, `context`, `rag`,
    /// `client_credentials`, `metrics`, `providers.<id>` (and its
    /// `tenant_overrides`), `thumbnail` and `knowledge_search`; ignores them inside
    /// the four worker sections; fails on the removed keys (`mcp`,
    /// `providers.<id>.supports_file_search_filters`, `streaming.web_search_context_size`).
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] naming the offending key or section.
    pub fn parse(raw: serde_json::Value) -> Result<Self, ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-removed-keys
        reject_removed_keys(&raw)?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-removed-keys
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-reject-unknown
        serde_json::from_value(raw).map_err(|e| ConfigError::Parse(format!("{e}")))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-reject-unknown
    }

    /// Validate every section after `${VAR}` expansion (ranges, required keys,
    /// provider rules, dangling `rag_provider`).
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] naming the offending key or section.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_top_level()?;
        self.validate_streaming_and_budgets()?;
        self.validate_quota_and_outbox()?;
        self.validate_context_and_rag()?;
        self.validate_knowledge_search()?;
        self.validate_workers()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-each-provider
        for (id, provider) in &self.providers {
            self.validate_provider(id, provider)?;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-each-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-return
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-return
    }

    /// [`parse`](Self::parse) followed by [`validate`](Self::validate), for a value that
    /// needs no expansion.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] naming the offending key or section.
    pub fn from_value(raw: serde_json::Value) -> Result<Self, ConfigError> {
        let cfg = Self::parse(raw)?;
        cfg.validate()?;
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-return
        Ok(cfg)
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-return
    }

    /// Dotted keys of the deprecated no-effect fields that are set to a non-default
    /// value (one startup warning each), for example `estimation_budgets.safety_margin_pct`
    /// or `cleanup_worker.batch_size`.
    #[must_use]
    pub fn deprecated_field_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut note = |key: &str, differs: bool| {
            if differs {
                warnings.push(format!("{key} is deprecated and has no effect"));
            }
        };

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-estimation-deprecated
        let (b, d) = (&self.estimation_budgets, EstimationBudgetsConfig::default());
        note(
            "estimation_budgets.bytes_per_token_conservative",
            b.bytes_per_token_conservative != d.bytes_per_token_conservative,
        );
        note(
            "estimation_budgets.fixed_overhead_tokens",
            b.fixed_overhead_tokens != d.fixed_overhead_tokens,
        );
        note(
            "estimation_budgets.safety_margin_pct",
            b.safety_margin_pct != d.safety_margin_pct,
        );
        note(
            "estimation_budgets.image_token_budget",
            b.image_token_budget != d.image_token_budget,
        );
        note(
            "estimation_budgets.tool_surcharge_tokens",
            b.tool_surcharge_tokens != d.tool_surcharge_tokens,
        );
        note(
            "estimation_budgets.web_search_surcharge_tokens",
            b.web_search_surcharge_tokens != d.web_search_surcharge_tokens,
        );
        note(
            "estimation_budgets.code_interpreter_surcharge_tokens",
            b.code_interpreter_surcharge_tokens != d.code_interpreter_surcharge_tokens,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-estimation-deprecated

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-deprecated-warnings
        let (c, d) = (&self.cleanup_worker, CleanupWorkerConfig::default());
        note("cleanup_worker.enabled", c.enabled != d.enabled);
        note(
            "cleanup_worker.poll_interval_secs",
            c.poll_interval_secs != d.poll_interval_secs,
        );
        note(
            "cleanup_worker.reconcile_interval_secs",
            c.reconcile_interval_secs != d.reconcile_interval_secs,
        );
        note(
            "cleanup_worker.stale_in_progress_timeout_secs",
            c.stale_in_progress_timeout_secs != d.stale_in_progress_timeout_secs,
        );
        note("cleanup_worker.batch_size", c.batch_size != d.batch_size);
        note(
            "thread_summary_worker.reconcile_interval_secs",
            self.thread_summary_worker.reconcile_interval_secs
                != ThreadSummaryWorkerConfig::default().reconcile_interval_secs,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-deprecated-warnings
        warnings
    }

    fn validate_top_level(&self) -> Result<(), ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-top-level
        require_non_empty("vendor", &self.vendor)?;
        require_non_empty(
            "client_credentials.client_id",
            &self.client_credentials.client_id,
        )?;
        require_non_empty(
            "client_credentials.client_secret",
            self.client_credentials.client_secret.expose_secret(),
        )?;
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-top-level
    }

    fn validate_streaming_and_budgets(&self) -> Result<(), ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-streaming
        let streaming = &self.streaming;
        in_range(
            "streaming.sse_ping_interval_seconds",
            streaming.sse_ping_interval_seconds,
            5,
            60,
        )?;
        in_range(
            "streaming.sse_channel_capacity",
            streaming.sse_channel_capacity,
            16,
            64,
        )?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-streaming
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-estimation-floor
        let floor = self.estimation_budgets.minimal_generation_floor;
        if floor == 0 || floor > streaming.max_output_tokens {
            return Err(invalid(
                "estimation_budgets.minimal_generation_floor",
                format!(
                    "must be greater than 0 and not greater than streaming.max_output_tokens ({}), got {floor}",
                    streaming.max_output_tokens
                ),
            ));
        }
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-estimation-floor
    }

    fn validate_quota_and_outbox(&self) -> Result<(), ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-quota
        let quota = &self.quota;
        in_range(
            "quota.overshoot_tolerance_factor",
            quota.overshoot_tolerance_factor,
            1.0,
            1.5,
        )?;
        in_range(
            "quota.warning_threshold_pct",
            quota.warning_threshold_pct,
            1,
            99,
        )?;
        positive(
            "quota.web_search_max_calls_per_message",
            quota.web_search_max_calls_per_message,
        )?;
        positive("quota.web_search_daily_quota", quota.web_search_daily_quota)?;
        positive(
            "quota.code_interpreter_max_calls_per_message",
            quota.code_interpreter_max_calls_per_message,
        )?;
        positive(
            "quota.code_interpreter_daily_quota",
            quota.code_interpreter_daily_quota,
        )?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-quota
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-outbox
        let outbox = &self.outbox;
        for (key, name) in [
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
            require_non_empty(key, name)?;
        }
        let partitions = outbox.num_partitions;
        if !partitions.is_power_of_two() || !(1..=64).contains(&partitions) {
            return Err(invalid(
                "outbox.num_partitions",
                format!("must be a power of 2 within 1..=64, got {partitions}"),
            ));
        }
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-outbox
    }

    fn validate_context_and_rag(&self) -> Result<(), ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-context
        in_range(
            "context.recent_messages_limit",
            self.context.recent_messages_limit,
            0,
            100,
        )?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-context
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-rag
        let rag = &self.rag;
        positive("rag.max_documents_per_chat", rag.max_documents_per_chat)?;
        positive(
            "rag.max_total_upload_mb_per_chat",
            rag.max_total_upload_mb_per_chat,
        )?;
        positive(
            "rag.uploaded_file_max_size_kb",
            rag.uploaded_file_max_size_kb,
        )?;
        positive(
            "rag.uploaded_image_max_size_kb",
            rag.uploaded_image_max_size_kb,
        )?;
        positive("rag.max_images_per_message", rag.max_images_per_message)?;
        in_range(
            "rag.max_concurrent_uploads",
            rag.max_concurrent_uploads,
            1,
            256,
        )?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-rag
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-thumbnail
        let thumbnail = &self.thumbnail;
        positive("thumbnail.width", thumbnail.width)?;
        positive("thumbnail.height", thumbnail.height)?;
        positive("thumbnail.max_bytes", thumbnail.max_bytes)?;
        positive("thumbnail.max_pixels", thumbnail.max_pixels)?;
        positive("thumbnail.max_decode_bytes", thumbnail.max_decode_bytes)?;
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-thumbnail
    }

    fn validate_knowledge_search(&self) -> Result<(), ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-knowledge-search
        let search = &self.knowledge_search;
        if search.enabled {
            let set = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.is_empty());
            if !set(&search.vector_store_id) {
                return Err(invalid(
                    "knowledge_search.vector_store_id",
                    "required when knowledge_search.enabled is true",
                ));
            }
            if !set(&search.provider_id) {
                return Err(invalid(
                    "knowledge_search.provider_id",
                    "required when knowledge_search.enabled is true",
                ));
            }
        }
        positive(
            "knowledge_search.max_calls_per_message",
            search.max_calls_per_message,
        )?;
        positive("knowledge_search.top_k", search.top_k)?;
        positive("knowledge_search.max_chunk_chars", search.max_chunk_chars)?;
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-knowledge-search
    }

    fn validate_workers(&self) -> Result<(), ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-workers
        let watchdog = &self.orphan_watchdog;
        in_range(
            "orphan_watchdog.timeout_secs",
            watchdog.timeout_secs,
            90,
            3600,
        )?;
        in_range(
            "orphan_watchdog.scan_interval_secs",
            watchdog.scan_interval_secs,
            1,
            3600,
        )?;
        let reaper = &self.upload_reaper;
        in_range(
            "upload_reaper.scan_interval_secs",
            reaper.scan_interval_secs,
            1,
            3600,
        )?;
        in_range(
            "upload_reaper.stale_after_secs",
            reaper.stale_after_secs,
            60,
            86_400,
        )?;
        let summary = &self.thread_summary_worker;
        in_range(
            "thread_summary_worker.claim_timeout_secs",
            summary.claim_timeout_secs,
            30,
            3600,
        )?;
        positive("thread_summary_worker.max_attempts", summary.max_attempts)?;
        in_range(
            "thread_summary_worker.compression_threshold_pct",
            summary.compression_threshold_pct,
            1,
            99,
        )?;
        positive(
            "cleanup_worker.max_attempts",
            self.cleanup_worker.max_attempts,
        )?;
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-workers
    }

    fn validate_provider(&self, id: &str, provider: &ProviderConfig) -> Result<(), ConfigError> {
        let key = |field: &str| format!("providers.{id}.{field}");
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-required
        // `kind`, `host` and `storage_kind` are required keys of the parse; the host is also
        // required to be non-empty
        require_non_empty(&key("host"), &provider.host)?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-required
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-host-chars
        check_host_chars(&key("host"), &provider.host)?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-host-chars
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-port
        if provider.port == Some(0) {
            return Err(invalid(key("port"), "must not be 0"));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-port
        if let Some(alias) = &provider.upstream_alias {
            check_alias_matches_derived(
                &key("upstream_alias"),
                alias,
                &provider.host,
                provider.effective_port(),
                provider.use_http,
            )?;
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-azure
        if provider.storage_kind == StorageKind::Azure {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-api-version
            check_api_version(&key("api_version"), provider.api_version.as_deref())?;
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-api-version
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-azure
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-rag-ref
        if let Some(rag_provider) = &provider.rag_provider
            && !self.providers.contains_key(rag_provider)
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-rag-error
            return Err(invalid(
                key("rag_provider"),
                format!("references the unknown provider `{rag_provider}`"),
            ));
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-rag-error
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-rag-ref
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-override
        for (tenant, tenant_override) in &provider.tenant_overrides {
            let override_key = format!("providers.{id}.tenant_overrides.{tenant}");
            if tenant_override.host.is_none() && tenant_override.upstream_alias.is_none() {
                return Err(invalid(
                    override_key,
                    "an override must set `host` or `upstream_alias`",
                ));
            }
            if let Some(host) = &tenant_override.host {
                check_host_chars(&format!("{override_key}.host"), host)?;
            }
            if let Some(alias) = &tenant_override.upstream_alias {
                check_alias_matches_derived(
                    &format!("{override_key}.upstream_alias"),
                    alias,
                    tenant_override.host.as_deref().unwrap_or(&provider.host),
                    provider.effective_port(),
                    provider.use_http,
                )?;
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-config-validation:p1:inst-config-provider-override
        Ok(())
    }
}

/// A config that still sets a removed key fails with a message that says so.
fn reject_removed_keys(raw: &serde_json::Value) -> Result<(), ConfigError> {
    let Some(root) = raw.as_object() else {
        return Ok(());
    };
    if root.contains_key("mcp") {
        return Err(invalid(
            "mcp",
            "removed key: MCP is deferred and not implemented",
        ));
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-removed
    // a startup error: the search context size comes from the catalog
    if root
        .get("streaming")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|streaming| streaming.contains_key("web_search_context_size"))
    {
        return Err(invalid(
            "streaming.web_search_context_size",
            "removed key: the web_search tool uses the catalog entry's web_search_context_size",
        ));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-removed
    if let Some(providers) = root.get("providers").and_then(serde_json::Value::as_object) {
        for (id, provider) in providers {
            if provider
                .as_object()
                .is_some_and(|entry| entry.contains_key("supports_file_search_filters"))
            {
                return Err(invalid(
                    format!("providers.{id}.supports_file_search_filters"),
                    "removed key",
                ));
            }
        }
    }
    Ok(())
}

fn require_non_empty(key: &str, value: &str) -> Result<(), ConfigError> {
    if value.is_empty() {
        Err(invalid(key, "must be non-empty"))
    } else {
        Ok(())
    }
}

fn in_range<T: PartialOrd + Display + Copy>(
    key: &str,
    value: T,
    min: T,
    max: T,
) -> Result<(), ConfigError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(invalid(
            key,
            format!("must be within {min}..={max}, got {value}"),
        ))
    }
}

fn positive<T: PartialOrd + Display + Copy + Default>(
    key: &str,
    value: T,
) -> Result<(), ConfigError> {
    if value > T::default() {
        Ok(())
    } else {
        Err(invalid(key, format!("must be greater than 0, got {value}")))
    }
}

/// The host becomes the OAGW alias in the proxied path, so only letters, digits and
/// `.`, `-`, `_`, `:`, `[`, `]` are allowed.
fn check_host_chars(key: &str, host: &str) -> Result<(), ConfigError> {
    let allowed =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']');
    if host.chars().all(allowed) {
        Ok(())
    } else {
        Err(invalid(
            key,
            "may contain only letters, digits and `.`, `-`, `_`, `:`, `[`, `]`",
        ))
    }
}

/// An Azure provider needs a non-blank `api_version` of letters, digits, `.` and `-` (it is
/// sent unencoded as the `api-version` query value).
fn check_api_version(key: &str, version: Option<&str>) -> Result<(), ConfigError> {
    let Some(version) = version.filter(|v| !v.trim().is_empty()) else {
        return Err(invalid(
            key,
            "required (non-blank) when storage_kind is azure",
        ));
    };
    if version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
    {
        Ok(())
    } else {
        Err(invalid(
            key,
            "may contain only letters, digits, `.` and `-`",
        ))
    }
}

fn expand_secret(secret: &mut SecretString) -> Result<(), ExpandVarsError> {
    secret.expand_vars()
}

fn expand_values(map: &mut BTreeMap<String, String>) -> Result<(), ExpandVarsError> {
    for value in map.values_mut() {
        *value = expand_env_vars(value)?;
    }
    Ok(())
}

impl ExpandVars for MiniChatConfig {
    /// Expand `${VAR}` only in provider `host` and `auth_config`, the same fields in
    /// `tenant_overrides`, and `client_credentials`.
    fn expand_vars(&mut self) -> Result<(), ExpandVarsError> {
        self.client_credentials.client_id.expand_vars()?;
        expand_secret(&mut self.client_credentials.client_secret)?;
        for provider in self.providers.values_mut() {
            provider.host.expand_vars()?;
            expand_values(&mut provider.auth_config)?;
            for tenant_override in provider.tenant_overrides.values_mut() {
                tenant_override.host.expand_vars()?;
                if let Some(auth_config) = tenant_override.auth_config.as_mut() {
                    expand_values(auth_config)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
