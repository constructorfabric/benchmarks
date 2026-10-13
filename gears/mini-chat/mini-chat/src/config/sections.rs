//! Plain configuration sections (everything except providers and workers).

use secrecy::SecretString;
use serde::Deserialize;
use toolkit_macros::ExpandVars;

/// S2S client credentials. Both values are required and must be non-empty.
#[derive(Debug, Clone, Deserialize, ExpandVars)]
#[serde(deny_unknown_fields)]
pub struct ClientCredentials {
    /// Client identifier.
    #[expand_vars]
    pub client_id: String,
    /// Client secret; redacted in logs.
    #[expand_vars]
    pub client_secret: SecretString,
}

/// Metrics settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix; empty means `mini_chat`.
    pub prefix: String,
}

/// Streaming (SSE) settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// Seconds between SSE pings before the first content event (`5..=60`).
    pub sse_ping_interval_seconds: u16,
    /// Capacity of the bounded channel between the provider task and the SSE writer (`16..=64`).
    pub sse_channel_capacity: u16,
    /// Cap on the applied output token limit.
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

/// Estimation budgets of the gear configuration.
///
/// Only `minimal_generation_floor` is used; the other fields are deprecated: parsed, not
/// validated and not used. Gear init logs a warning for each one set to a non-default value.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EstimationBudgetsConfig {
    /// Floor of the bounded debit for aborted turns; `> 0` and `<= streaming.max_output_tokens`.
    pub minimal_generation_floor: u32,
    /// Deprecated, not used.
    pub bytes_per_token_conservative: u32,
    /// Deprecated, not used.
    pub fixed_overhead_tokens: u32,
    /// Deprecated, not used.
    pub safety_margin_pct: u32,
    /// Deprecated, not used.
    pub image_token_budget: u32,
    /// Deprecated, not used.
    pub tool_surcharge_tokens: u32,
    /// Deprecated, not used.
    pub web_search_surcharge_tokens: u32,
    /// Deprecated, not used.
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

/// Quota knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    /// Overshoot tolerance factor (`1.0..=1.5`).
    pub overshoot_tolerance_factor: f64,
    /// Percentage of used quota at which a warning fires (`1..=99`).
    pub warning_threshold_pct: u8,
    /// Hard limit on `web_search` calls in one turn (`> 0`).
    pub web_search_max_calls_per_message: u32,
    /// Per-user daily web search call limit (`> 0`).
    pub web_search_daily_quota: u32,
    /// Hard limit on `code_interpreter` calls in one turn (`> 0`).
    pub code_interpreter_max_calls_per_message: u32,
    /// Per-user daily code interpreter call limit (`> 0`).
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

/// Shared outbox integration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutboxConfig {
    /// Usage events queue.
    pub queue_name: String,
    /// Attachment deletion queue.
    pub cleanup_queue_name: String,
    /// Chat deletion queue.
    pub chat_cleanup_queue_name: String,
    /// Thread summary queue.
    pub thread_summary_queue_name: String,
    /// Audit events queue.
    pub audit_queue_name: String,
    /// Partitions of every queue: a power of two in `1..=64`.
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

/// Built-in guard instruction appended to the system prompt when `web_search` is sent.
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained \
from the provided context or your training data. Never use it for general knowledge questions. \
At most one web_search call per request.";

/// Built-in guard instruction appended to the system prompt when `file_search` is sent.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search only for questions about the \
documents the user uploaded to this chat. Never use it for general knowledge questions.";

/// Built-in guard instruction appended to the system prompt when `search_knowledge` is sent.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to look up information \
in the organization knowledge base when a question depends on internal documentation. Never \
use it for general knowledge questions.";

/// Context assembly settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Recent messages kept in the context (`0..=100`).
    pub recent_messages_limit: u32,
    /// Guard instruction for the `web_search` tool.
    pub web_search_guard: String,
    /// Guard instruction for the `file_search` tool.
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

/// File search / RAG and upload limits.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RagConfig {
    /// Documents per chat (`> 0`).
    pub max_documents_per_chat: u32,
    /// Total upload size per chat in MiB, images included (`> 0`).
    pub max_total_upload_mb_per_chat: u32,
    /// Accept `text/csv` (remapped to `text/plain`).
    pub allow_csv_upload: bool,
    /// Concurrent uploads per process (`1..=256`).
    pub max_concurrent_uploads: u16,
    /// Document size limit in KiB (`> 0`).
    pub uploaded_file_max_size_kb: u32,
    /// Image size limit in KiB (`> 0`).
    pub uploaded_image_max_size_kb: u32,
    /// Images per message (`> 0`).
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

/// Image thumbnail settings; every value must be `> 0`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThumbnailConfig {
    /// Thumbnail width in pixels.
    pub width: u32,
    /// Thumbnail height in pixels.
    pub height: u32,
    /// Maximum thumbnail size in bytes.
    pub max_bytes: usize,
    /// Maximum source image pixels.
    pub max_pixels: u64,
    /// Maximum source image bytes that are decoded.
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
    /// Provider vector store; required when enabled.
    pub vector_store_id: Option<String>,
    /// Provider entry serving the vector store; required when enabled.
    pub provider_id: Option<String>,
    /// Retrievals per message (`> 0`).
    pub max_calls_per_message: u32,
    /// Chunks per retrieval (`> 0`).
    pub top_k: usize,
    /// Characters kept per chunk (`> 0`).
    pub max_chunk_chars: usize,
    /// Guard instruction for the `search_knowledge` tool.
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
