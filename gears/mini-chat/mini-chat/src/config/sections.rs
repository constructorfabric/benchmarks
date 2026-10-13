//! Simple config sections (DESIGN B.4 - B.9). Strict sections reject unknown
//! keys; the worker sections ignore them.

use serde::Deserialize;

use super::validate::{non_empty, positive, range};

/// Built-in `web_search` guard (DESIGN 4 "Web Search Configuration").
pub const DEFAULT_WEB_SEARCH_GUARD: &str = "Use web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request.";

/// Built-in `file_search` guard.
pub const DEFAULT_FILE_SEARCH_GUARD: &str = "Use file_search only when the answer depends on the documents the user uploaded to this chat. Do not use it for general knowledge questions.";

/// Built-in `search_knowledge` guard.
pub const DEFAULT_KNOWLEDGE_SEARCH_GUARD: &str = "Use search_knowledge to look up organization knowledge before answering questions that depend on it. Answer from the returned chunks and say so when nothing relevant is found.";

/// Built-in system prompt of the thread-summary request (B.5.5).
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// `metrics` section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Metric name prefix (`""` = `mini_chat`).
    pub prefix: String,
}

/// `streaming` section (B.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamingConfig {
    /// Seconds between SSE pings, `5..=60`.
    pub sse_ping_interval_seconds: u16,
    /// Capacity of the bounded provider -> SSE channel, `16..=64`.
    pub sse_channel_capacity: u16,
    /// Cap on `max_output_tokens_applied`.
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

impl StreamingConfig {
    pub(super) fn validate(&self) -> Result<(), String> {
        range(
            "streaming.sse_ping_interval_seconds",
            self.sse_ping_interval_seconds,
            5,
            60,
        )?;
        range(
            "streaming.sse_channel_capacity",
            self.sse_channel_capacity,
            16,
            64,
        )
    }
}

/// `estimation_budgets` section (B.5.2): only `minimal_generation_floor` is
/// used, the other keys are deprecated (parsed, not validated, not used).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EstimationBudgetsConfig {
    /// Lower bound of the generation budget, `> 0` and `<= streaming.max_output_tokens`.
    pub minimal_generation_floor: u32,
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
    pub(super) fn validate(&self, streaming: &StreamingConfig) -> Result<(), String> {
        positive(
            "estimation_budgets.minimal_generation_floor",
            self.minimal_generation_floor,
        )?;
        if self.minimal_generation_floor > streaming.max_output_tokens {
            return Err(format!(
                "estimation_budgets.minimal_generation_floor ({}) must be <= streaming.max_output_tokens ({})",
                self.minimal_generation_floor, streaming.max_output_tokens
            ));
        }
        Ok(())
    }

    pub(super) fn deprecated_non_default_keys(&self) -> Vec<&'static str> {
        let defaults = Self::default();
        [
            (
                "bytes_per_token_conservative",
                self.bytes_per_token_conservative != defaults.bytes_per_token_conservative,
            ),
            (
                "fixed_overhead_tokens",
                self.fixed_overhead_tokens != defaults.fixed_overhead_tokens,
            ),
            (
                "safety_margin_pct",
                self.safety_margin_pct != defaults.safety_margin_pct,
            ),
            (
                "image_token_budget",
                self.image_token_budget != defaults.image_token_budget,
            ),
            (
                "tool_surcharge_tokens",
                self.tool_surcharge_tokens != defaults.tool_surcharge_tokens,
            ),
            (
                "web_search_surcharge_tokens",
                self.web_search_surcharge_tokens != defaults.web_search_surcharge_tokens,
            ),
            (
                "code_interpreter_surcharge_tokens",
                self.code_interpreter_surcharge_tokens
                    != defaults.code_interpreter_surcharge_tokens,
            ),
        ]
        .into_iter()
        .filter_map(|(name, changed)| changed.then_some(name))
        .collect()
    }
}

/// `quota` section (B.5.3, B.6).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QuotaConfig {
    /// Overshoot tolerance, `1.0..=1.5`.
    pub overshoot_tolerance_factor: f64,
    /// Warning threshold in percent, `1..=99`.
    pub warning_threshold_pct: u8,
    /// Hard limit of `web_search` calls per message, `> 0`.
    pub web_search_max_calls_per_message: u32,
    /// Daily `web_search` quota, `> 0`.
    pub web_search_daily_quota: u32,
    /// Hard limit of `code_interpreter` calls per message, `> 0`.
    pub code_interpreter_max_calls_per_message: u32,
    /// Daily `code_interpreter` quota, `> 0`.
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
    pub(super) fn validate(&self) -> Result<(), String> {
        range(
            "quota.overshoot_tolerance_factor",
            self.overshoot_tolerance_factor,
            1.0,
            1.5,
        )?;
        range(
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

/// `outbox` section (B.9.3): the five leased queues.
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
    /// Partitions per queue (power of two, `1..=64`).
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
    pub(super) fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("queue_name", &self.queue_name),
            ("cleanup_queue_name", &self.cleanup_queue_name),
            ("chat_cleanup_queue_name", &self.chat_cleanup_queue_name),
            ("thread_summary_queue_name", &self.thread_summary_queue_name),
            ("audit_queue_name", &self.audit_queue_name),
        ] {
            non_empty(&format!("outbox.{name}"), value)?;
        }
        if !self.num_partitions.is_power_of_two() || self.num_partitions > 64 {
            return Err(format!(
                "outbox.num_partitions must be a power of two in 1..=64, got {}",
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
    /// Recent messages kept in the context, `0..=100`.
    pub recent_messages_limit: u32,
    /// Guard appended to the system prompt when `web_search` is sent.
    pub web_search_guard: String,
    /// Guard appended to the system prompt when `file_search` is sent.
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
    pub(super) fn validate(&self) -> Result<(), String> {
        range(
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
    /// Documents per chat, `> 0`.
    pub max_documents_per_chat: u32,
    /// Total upload size per chat in MB, `> 0`.
    pub max_total_upload_mb_per_chat: u32,
    /// Accept `text/csv` (remapped to `text/plain`).
    pub allow_csv_upload: bool,
    /// Concurrent uploads per process, `1..=256`.
    pub max_concurrent_uploads: u16,
    /// Document size limit in KiB, `> 0`.
    pub uploaded_file_max_size_kb: u32,
    /// Image size limit in KiB, `> 0`.
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
            uploaded_file_max_size_kb: 25_600,
            uploaded_image_max_size_kb: 5_120,
            max_images_per_message: 4,
        }
    }
}

impl RagConfig {
    pub(super) fn validate(&self) -> Result<(), String> {
        positive("rag.max_documents_per_chat", self.max_documents_per_chat)?;
        positive(
            "rag.max_total_upload_mb_per_chat",
            self.max_total_upload_mb_per_chat,
        )?;
        range(
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
    /// Thumbnail width in pixels, `> 0`.
    pub width: u32,
    /// Thumbnail height in pixels, `> 0`.
    pub height: u32,
    /// Maximum encoded thumbnail size, `> 0`.
    pub max_bytes: usize,
    /// Maximum decoded pixel count, `> 0`.
    pub max_pixels: u64,
    /// Maximum source size for decoding, `> 0`.
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
    pub(super) fn validate(&self) -> Result<(), String> {
        positive("thumbnail.width", self.width)?;
        positive("thumbnail.height", self.height)?;
        positive("thumbnail.max_bytes", self.max_bytes)?;
        positive("thumbnail.max_pixels", self.max_pixels)?;
        positive("thumbnail.max_decode_bytes", self.max_decode_bytes)
    }
}

/// `orphan_watchdog` section (B.9.1). Unknown keys are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrphanWatchdogConfig {
    /// Run the watchdog.
    pub enabled: bool,
    /// Seconds without progress before a running turn is orphaned, `90..=3600`.
    pub timeout_secs: u64,
    /// Scan interval in seconds, `1..=3600`.
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
    pub(super) fn validate(&self) -> Result<(), String> {
        range("orphan_watchdog.timeout_secs", self.timeout_secs, 90, 3600)?;
        range(
            "orphan_watchdog.scan_interval_secs",
            self.scan_interval_secs,
            1,
            3600,
        )
    }
}

/// `upload_reaper` section (B.9.5). Unknown keys are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UploadReaperConfig {
    /// Run the reaper.
    pub enabled: bool,
    /// Scan interval in seconds, `1..=3600`.
    pub scan_interval_secs: u64,
    /// Age after which a pending upload is abandoned, `60..=86400`.
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
    pub(super) fn validate(&self) -> Result<(), String> {
        range(
            "upload_reaper.scan_interval_secs",
            self.scan_interval_secs,
            1,
            3600,
        )?;
        range(
            "upload_reaper.stale_after_secs",
            self.stale_after_secs,
            60,
            86_400,
        )
    }
}

/// `thread_summary_worker` section (B.9.4). Unknown keys are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThreadSummaryWorkerConfig {
    /// Run the worker.
    pub enabled: bool,
    /// Outbox lease in seconds, `30..=3600`.
    pub claim_timeout_secs: u64,
    /// Attempts before the message is rejected, `> 0`.
    pub max_attempts: u32,
    /// Context fill in percent that triggers a summary, `1..=99`.
    pub compression_threshold_pct: u32,
    /// Model used for summaries (`""` = the chat model).
    pub summary_model_id: String,
    /// System prompt of the summary request.
    pub summary_system_prompt: String,
    /// Per-message truncation limit (0 = none).
    pub message_content_limit: usize,
    /// Reconcile interval in seconds.
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
    pub(super) fn validate(&self) -> Result<(), String> {
        range(
            "thread_summary_worker.claim_timeout_secs",
            self.claim_timeout_secs,
            30,
            3600,
        )?;
        positive("thread_summary_worker.max_attempts", self.max_attempts)?;
        range(
            "thread_summary_worker.compression_threshold_pct",
            self.compression_threshold_pct,
            1,
            99,
        )
    }
}

/// `cleanup_worker` section (B.9.2). Unknown keys are ignored.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CleanupWorkerConfig {
    /// Attempts before a cleanup message is rejected, `> 0`.
    pub max_attempts: u32,
    /// Run the cleanup workers.
    pub enabled: bool,
    /// Poll interval in seconds.
    pub poll_interval_secs: u64,
    /// Reconcile interval in seconds.
    pub reconcile_interval_secs: u64,
    /// Age after which an in-progress cleanup is reclaimed, in seconds.
    pub stale_in_progress_timeout_secs: u64,
    /// Batch size.
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
    pub(super) fn validate(&self) -> Result<(), String> {
        positive("cleanup_worker.max_attempts", self.max_attempts)
    }
}

/// `knowledge_search` section (B.7).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KnowledgeSearchConfig {
    /// Enable knowledge search.
    pub enabled: bool,
    /// Vector store holding the knowledge base (required when enabled).
    pub vector_store_id: Option<String>,
    /// Provider entry serving the vector store (required when enabled).
    pub provider_id: Option<String>,
    /// Retrievals per message, `> 0` when enabled.
    pub max_calls_per_message: u32,
    /// Chunks returned per retrieval, `> 0` when enabled.
    pub top_k: usize,
    /// Characters kept per chunk, `> 0` when enabled.
    pub max_chunk_chars: usize,
    /// Guard appended to the system prompt.
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
    pub(super) fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        non_empty(
            "knowledge_search.vector_store_id",
            self.vector_store_id.as_deref().unwrap_or_default(),
        )?;
        non_empty(
            "knowledge_search.provider_id",
            self.provider_id.as_deref().unwrap_or_default(),
        )?;
        positive(
            "knowledge_search.max_calls_per_message",
            self.max_calls_per_message,
        )?;
        positive("knowledge_search.top_k", self.top_k)?;
        positive("knowledge_search.max_chunk_chars", self.max_chunk_chars)
    }
}
