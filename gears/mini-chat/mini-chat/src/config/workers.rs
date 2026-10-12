//! Worker sections. They accept and ignore unknown keys.

use serde::Deserialize;

/// Built-in system prompt of the thread-summary request.
pub const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a \
conversation (and optionally an existing summary), produce a detailed structured summary. \
Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final \
summary). Only the <summary> content will be stored. Do not invent information not present in \
the conversation.";

/// Catalog model used for summaries when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Orphan watchdog worker.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OrphanWatchdogConfig {
    /// Run the watchdog.
    pub enabled: bool,
    /// Seconds without progress after which a running turn is an orphan (`90..=3600`).
    pub timeout_secs: u64,
    /// Seconds between scans (`1..=3600`).
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

/// Upload reaper worker.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UploadReaperConfig {
    /// Run the reaper.
    pub enabled: bool,
    /// Seconds between scans (`1..=3600`).
    pub scan_interval_secs: u64,
    /// Seconds after which a `pending`/`uploaded` attachment is abandoned (`60..=86400`).
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

/// Thread summary worker.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThreadSummaryWorkerConfig {
    /// Gates the summary trigger in finalization.
    pub enabled: bool,
    /// Outbox lease of the thread-summary queue in seconds (`30..=3600`).
    pub claim_timeout_secs: u64,
    /// Deliveries before the task is dead-lettered (`> 0`).
    pub max_attempts: u32,
    /// Context fill percentage that triggers a summary (`1..=99`).
    pub compression_threshold_pct: u32,
    /// Catalog model used for summaries; empty means `gpt-4.1-mini`.
    pub summary_model_id: String,
    /// Fallback system prompt when the catalog `thread_summary_prompt` is empty.
    pub summary_system_prompt: String,
    /// Maximum characters per message in the prompt; `0` means no truncation.
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
    /// The catalog model used for summaries.
    #[must_use]
    pub fn effective_summary_model_id(&self) -> &str {
        if self.summary_model_id.is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            &self.summary_model_id
        }
    }
}

/// Cleanup worker. Cleanup runs as outbox handlers; only `max_attempts` has an effect.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CleanupWorkerConfig {
    /// Deprecated, no effect.
    pub enabled: bool,
    /// Provider delete attempts per attachment before cleanup becomes terminal `failed` (`> 0`).
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
