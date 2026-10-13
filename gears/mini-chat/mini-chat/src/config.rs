//! Gear configuration (`gears.mini-chat.config`): sections, defaults and validation.
//!
//! Implements `cpt-cf-mini-chat-algo-gear-foundation-load-config`. Secrets
//! (`client_credentials.client_secret` and every provider `auth_config` value)
//! are wrapped in [`SecretValue`] and never appear in `Debug` output or error
//! messages.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-configuration:p1

/// Implements the scalar `Visitor` methods of a secret-carrying type so that a
/// type mismatch reports only the expected shape, never the offending value
/// (serde's default `invalid_type` message echoes integers, strings and bools).
macro_rules! reject_scalars_without_echo {
    ($message:literal) => {
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Err(E::custom($message))
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Err(E::custom($message))
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Err(E::custom($message))
        }
        fn visit_i128<E: serde::de::Error>(self, _: i128) -> Result<Self::Value, E> {
            Err(E::custom($message))
        }
        fn visit_u128<E: serde::de::Error>(self, _: u128) -> Result<Self::Value, E> {
            Err(E::custom($message))
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Err(E::custom($message))
        }
        fn visit_bytes<E: serde::de::Error>(self, _: &[u8]) -> Result<Self::Value, E> {
            Err(E::custom($message))
        }
    };
}

mod providers;
mod validation;

pub use providers::{
    AuthConfig, OrderedMap, ProviderConfig, ProviderKind, ProviderMap, ProviderRegistry,
    ProviderTarget, ResolvedProvider, ResolvedTenantOverride, StorageKind, TenantOverrideConfig,
    derived_alias, effective_alias, is_ip_literal, normalize_alias, validate_providers,
};

use std::fmt;

use serde::Deserialize;

/// Built-in web search guard (appended to the system prompt, DESIGN section 4).
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";
/// Built-in file search guard.
pub const DEFAULT_FILE_SEARCH_GUARD: &str =
    "Use file_search only when the question refers to the content of the attached documents.";
/// Built-in knowledge search guard.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str =
    "Use search_knowledge only when the question requires information from the knowledge base.";
/// Built-in thread summary system prompt (DESIGN B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// A secret string: redacted in `Debug`, no `Display`, no serialization.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SecretValue(String);

impl<'de> Deserialize<'de> for SecretValue {
    /// Accepts only a string; any other type fails with a value-free message.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SecretVisitor;

        impl serde::de::Visitor<'_> for SecretVisitor {
            type Value = SecretValue;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a secret string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(SecretValue(value.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(SecretValue(value))
            }

            reject_scalars_without_echo!("secret value must be a string");
        }

        deserializer.deserialize_str(SecretVisitor)
    }
}

impl SecretValue {
    /// Wraps a secret.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the secret text. Callers must never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretValue([REDACTED])")
    }
}

/// Error returned by configuration loading and validation.
///
/// Messages name only the key path and the rule, never a secret value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The raw section could not be parsed (unknown key, wrong type, missing field).
    #[error("invalid mini-chat configuration: {0}")]
    Parse(String),
    /// A key violates a validation rule.
    #[error("invalid mini-chat configuration `{key}`: {rule}")]
    Invalid {
        /// Key path, for example `streaming.sse_ping_interval_seconds`.
        key: String,
        /// Violated rule.
        rule: String,
    },
    /// Two targets share an upstream alias but differ in connection settings.
    #[error(
        "invalid mini-chat configuration: upstream alias `{alias}` is shared by `{first}` and `{second}` but they differ in: {}",
        fields.join(", ")
    )]
    AliasConflict {
        /// The shared alias.
        alias: String,
        /// First target (`providers.<id>` or `providers.<id>.tenant_overrides.<tenant>`).
        first: String,
        /// Later conflicting target.
        second: String,
        /// Names of the differing fields.
        fields: Vec<String>,
    },
}

impl ConfigError {
    /// Builds a validation error for `key` violating `rule`. Neither may carry a secret.
    pub(crate) fn invalid(key: impl Into<String>, rule: impl Into<String>) -> Self {
        Self::Invalid {
            key: key.into(),
            rule: rule.into(),
        }
    }
}

/// A non-fatal startup warning (deprecated key set to a non-default value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    /// Key path of the deprecated field, for example `cleanup_worker.batch_size`.
    pub key: String,
    /// Warning text.
    pub message: String,
}

/// Result of [`MiniChatConfig::load`].
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Validated configuration with defaults applied and `${VAR}` expanded.
    pub config: MiniChatConfig,
    /// Validated provider entries and effective OAGW targets.
    pub providers: ProviderRegistry,
    /// Startup warnings.
    pub warnings: Vec<ConfigWarning>,
}

/// S2S credentials exchanged through `authn_resolver`.
#[derive(Debug, Clone, Default)]
pub struct ClientCredentials {
    /// Client id (`${VAR}` expanded, non-empty).
    pub client_id: String,
    /// Client secret (`${VAR}` expanded, non-empty, redacted).
    pub client_secret: SecretValue,
}

/// Field-level mirror of [`ClientCredentials`] used for deserialization.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ClientCredentialsFields {
    client_id: String,
    client_secret: SecretValue,
}

impl<'de> Deserialize<'de> for ClientCredentials {
    /// A non-map input fails with a value-free message (serde's default message
    /// would echo a string input, which may be a pasted secret).
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let value = serde_json::Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Err(D::Error::custom("client_credentials must be a map"));
        }
        let fields = ClientCredentialsFields::deserialize(value)
            .map_err(|error| D::Error::custom(redact_parse_message(&error.to_string())))?;
        Ok(Self {
            client_id: fields.client_id,
            client_secret: fields.client_secret,
        })
    }
}

/// Metrics settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix; empty means `mini_chat`.
    pub prefix: String,
}

/// SSE streaming settings (DESIGN B.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// Ping interval, `5..=60`.
    pub sse_ping_interval_seconds: u16,
    /// Channel capacity, `16..=64`.
    pub sse_channel_capacity: u16,
    /// Cap on applied output tokens.
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

/// Estimation budgets (only `minimal_generation_floor` is used; the rest is deprecated).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EstimationBudgetsConfig {
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
    /// Minimal generation floor, `> 0` and `<= streaming.max_output_tokens`.
    pub minimal_generation_floor: u32,
}

impl Default for EstimationBudgetsConfig {
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

/// Quota knobs (DESIGN B.5.3, B.6, B.6.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    /// Overshoot tolerance, `1.0..=1.5`.
    pub overshoot_tolerance_factor: f64,
    /// Warning threshold percent, `1..=99`.
    pub warning_threshold_pct: u8,
    /// Web search calls per message, `> 0`.
    pub web_search_max_calls_per_message: u32,
    /// Web search calls per user per day, `> 0`.
    pub web_search_daily_quota: u32,
    /// Code interpreter calls per message, `> 0`.
    pub code_interpreter_max_calls_per_message: u32,
    /// Code interpreter calls per user per day, `> 0`.
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

/// Context assembly settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Recent messages kept in context, `0..=100`.
    pub recent_messages_limit: u32,
    /// Web search guard text.
    pub web_search_guard: String,
    /// File search guard text.
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

/// File search / RAG and upload limits (DESIGN B.7, B.8).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RagConfig {
    /// Documents per chat, `> 0`.
    pub max_documents_per_chat: u32,
    /// Total upload MiB per chat, `> 0`.
    pub max_total_upload_mb_per_chat: u32,
    /// Accept `text/csv`.
    pub allow_csv_upload: bool,
    /// Concurrent uploads per process, `1..=256`.
    pub max_concurrent_uploads: u16,
    /// Document size cap in KiB, `> 0`.
    pub uploaded_file_max_size_kb: u32,
    /// Image size cap in KiB, `> 0`.
    pub uploaded_image_max_size_kb: u32,
    /// Images per message, `> 0`.
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

/// Thumbnail generation limits, each `> 0`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThumbnailConfig {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Maximum thumbnail bytes.
    pub max_bytes: usize,
    /// Maximum source pixels.
    pub max_pixels: u64,
    /// Maximum source bytes decoded.
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

/// Knowledge search settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KnowledgeSearchConfig {
    /// Feature switch.
    pub enabled: bool,
    /// Vector store (required when enabled).
    pub vector_store_id: Option<String>,
    /// Provider (required when enabled).
    pub provider_id: Option<String>,
    /// Calls per message, `> 0`.
    pub max_calls_per_message: u32,
    /// Top-k, `> 0`.
    pub top_k: usize,
    /// Maximum characters per chunk, `> 0`.
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
            guard: DEFAULT_KNOWLEDGE_SEARCH_GUARD.to_owned(),
        }
    }
}

/// Orphan watchdog worker (unknown keys accepted).
#[derive(Debug, Clone, Deserialize)]
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-accept-worker-unknown
#[serde(default)]
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-accept-worker-unknown
pub struct OrphanWatchdogConfig {
    /// Feature switch.
    pub enabled: bool,
    /// Stale-progress timeout, `90..=3600`.
    pub timeout_secs: u64,
    /// Scan interval, `1..=3600`.
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

/// Upload reaper worker (unknown keys accepted).
#[derive(Debug, Clone, Deserialize)]
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-accept-worker-unknown
#[serde(default)]
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-accept-worker-unknown
pub struct UploadReaperConfig {
    /// Feature switch.
    pub enabled: bool,
    /// Scan interval, `1..=3600`.
    pub scan_interval_secs: u64,
    /// Stale threshold, `60..=86400`.
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

/// Thread summary worker (unknown keys accepted).
#[derive(Debug, Clone, Deserialize)]
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-accept-worker-unknown
#[serde(default)]
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-accept-worker-unknown
pub struct ThreadSummaryWorkerConfig {
    /// Gates the trigger in finalization.
    pub enabled: bool,
    /// Outbox lease, `30..=3600`.
    pub claim_timeout_secs: u64,
    /// Deliveries before dead-letter, `> 0`.
    pub max_attempts: u32,
    /// Proactive trigger threshold, `1..=99`.
    pub compression_threshold_pct: u32,
    /// Catalog model used for summaries; empty means `gpt-4.1-mini`.
    pub summary_model_id: String,
    /// Fallback summary system prompt.
    pub summary_system_prompt: String,
    /// Maximum characters per message in the prompt; 0 means no truncation.
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

/// Cleanup worker (unknown keys accepted; most fields deprecated).
#[derive(Debug, Clone, Deserialize)]
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-accept-worker-unknown
#[serde(default)]
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-accept-worker-unknown
pub struct CleanupWorkerConfig {
    /// Deprecated, no effect.
    pub enabled: bool,
    /// Provider delete attempts, `> 0`.
    pub max_attempts: u32,
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
            enabled: true,
            max_attempts: 5,
            poll_interval_secs: 60,
            reconcile_interval_secs: 300,
            stale_in_progress_timeout_secs: 900,
            batch_size: 32,
        }
    }
}

/// Outbox queue names and partitioning.
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
    /// Partitions per queue, a power of two in `1..=64`.
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

/// The `mini-chat` gear configuration. Unknown top-level keys (including `mcp`)
/// are rejected.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MiniChatConfig {
    /// Prefix of all REST routes.
    pub url_prefix: String,
    /// Vendor selecting the policy and audit plugin instances (non-empty).
    pub vendor: String,
    /// S2S credentials (required, non-empty).
    pub client_credentials: ClientCredentials,
    /// Metrics settings.
    pub metrics: MetricsConfig,
    /// Streaming settings.
    pub streaming: StreamingConfig,
    /// Estimation budgets.
    pub estimation_budgets: EstimationBudgetsConfig,
    /// Quota knobs.
    pub quota: QuotaConfig,
    /// Context settings.
    pub context: ContextConfig,
    /// RAG and upload limits.
    pub rag: RagConfig,
    /// Thumbnail limits.
    pub thumbnail: ThumbnailConfig,
    /// Knowledge search.
    pub knowledge_search: KnowledgeSearchConfig,
    /// Orphan watchdog worker.
    pub orphan_watchdog: OrphanWatchdogConfig,
    /// Upload reaper worker.
    pub upload_reaper: UploadReaperConfig,
    /// Thread summary worker.
    pub thread_summary_worker: ThreadSummaryWorkerConfig,
    /// Cleanup worker.
    pub cleanup_worker: CleanupWorkerConfig,
    /// Outbox queues.
    pub outbox: OutboxConfig,
    /// Raw provider entries; `None` means the single default entry `openai`.
    pub providers: Option<ProviderMap>,
}

impl Default for MiniChatConfig {
    fn default() -> Self {
        Self {
            url_prefix: "/mini-chat".to_owned(),
            vendor: "constructorfabric".to_owned(),
            client_credentials: ClientCredentials::default(),
            metrics: MetricsConfig::default(),
            streaming: StreamingConfig::default(),
            estimation_budgets: EstimationBudgetsConfig::default(),
            quota: QuotaConfig::default(),
            context: ContextConfig::default(),
            rag: RagConfig::default(),
            thumbnail: ThumbnailConfig::default(),
            knowledge_search: KnowledgeSearchConfig::default(),
            orphan_watchdog: OrphanWatchdogConfig::default(),
            upload_reaper: UploadReaperConfig::default(),
            thread_summary_worker: ThreadSummaryWorkerConfig::default(),
            cleanup_worker: CleanupWorkerConfig::default(),
            outbox: OutboxConfig::default(),
            providers: None,
        }
    }
}

/// Strips the offending value from serde `invalid type: ...` / `invalid value: ...`
/// messages (defense in depth: such a value may be a secret). `unknown field` and
/// `missing field` messages are kept as they name keys only.
fn redact_parse_message(message: &str) -> String {
    for label in ["invalid type: ", "invalid value: "] {
        let Some(start) = message.find(label) else {
            continue;
        };
        let head = &message[..start + label.len()];
        let rest = &message[start + label.len()..];
        let (value, tail) = match rest.rfind(", expected ") {
            Some(index) => rest.split_at(index),
            None => (rest, ""),
        };
        let kept = value.find(['`', '"']).map_or(value, |at| &value[..at]);
        return format!("{head}{}{tail}", kept.trim_end());
    }
    message.to_owned()
}

impl MiniChatConfig {
    /// Parses, expands (`${VAR}` in `client_credentials` and provider / override
    /// `host` and `auth_config` only) and validates the raw `mini-chat` section.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] naming the key path and the violated rule for an
    /// unknown key, a removed key, a missing required key or an out-of-range value.
    pub fn load(raw: &serde_json::Value) -> Result<LoadedConfig, ConfigError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-if-any-failed
        let loaded = match Self::parse_and_validate(raw) {
            Ok(loaded) => loaded,
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-return-validation-error
                tracing::error!(%error, "mini-chat configuration is invalid");
                return Err(error);
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-return-validation-error
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-if-any-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-return-config
        Ok(loaded)
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-return-config
    }

    /// Runs steps 1 to 20 of the loading algorithm and stops at the first failure.
    fn parse_and_validate(raw: &serde_json::Value) -> Result<LoadedConfig, ConfigError> {
        validation::reject_removed_keys(raw)?;

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-reject-unknown
        let mut config: Self = serde_json::from_value(raw.clone())
            .map_err(|error| ConfigError::Parse(redact_parse_message(&error.to_string())))?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-reject-unknown

        validation::expand_client_credentials(&mut config.client_credentials)?;
        let warnings = validation::validate_sections(&config)?;

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-providers
        let providers = validate_providers(config.providers.as_ref())?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-load-config:p1:inst-validate-providers

        Ok(LoadedConfig {
            config,
            providers,
            warnings: warnings.into_vec(),
        })
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
