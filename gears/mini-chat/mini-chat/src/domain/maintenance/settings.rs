//! The values the two handlers read (`cpt-cf-mini-chat-algo-background-maintenance-configuration`):
//! `thread_summary_worker` and `cleanup_worker` keys plus the fixed values that stay out of the
//! configuration.

use std::time::Duration;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::MiniChatConfig;

// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-fixed
// the fixed values stay out of the configuration: at most 2 prompt-too-long retries, drops of one
// fifth of the kept messages with at least two kept, the `ceil(bytes / 4)` estimate fallback, the
// 10 s margin below the summary lease, the system subject id, the prompt texts, the read bounds of
// the summary call and, for chat cleanup, the 5 s deadline of every provider call and the 20 s
// time budget of one delivery
/// The summary model when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// The provider phase of a summary delivery ends this long before the queue lease.
pub const SUMMARY_LEASE_MARGIN: Duration = Duration::from_secs(10);

/// At most this many prompt-too-long retries per delivery.
pub const MAX_PROMPT_TOO_LONG_RETRIES: u32 = 2;

/// The fixed deadline of every provider call of the chat-cleanup handler.
pub const PROVIDER_CALL_DEADLINE: Duration = Duration::from_secs(5);

/// The fixed time budget of one chat-cleanup delivery: no provider call starts after it.
pub const DELIVERY_BUDGET: Duration = Duration::from_secs(20);

/// The read bound of a successful summary answer body.
pub const MAX_SUCCESS_BODY_BYTES: usize = 1 << 20;

/// The read bound of an error body.
pub const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

/// The platform default subject id the summary request carries as its user (system identity).
pub const SYSTEM_SUBJECT_ID: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-fixed

/// `thread_summary_worker` as the summary handler reads it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryWorkerSettings {
    /// `summary_model_id`; empty means [`DEFAULT_SUMMARY_MODEL_ID`] (see [`Self::model_id`]).
    pub summary_model_id: String,
    /// `summary_system_prompt`; empty falls back to the built-in default.
    pub summary_system_prompt: String,
    /// `message_content_limit`; 0 means no truncation.
    pub message_content_limit: usize,
    /// `max_attempts` (greater than 0).
    pub max_attempts: u32,
    /// `claim_timeout_secs` as a duration: also the queue lease.
    pub claim_timeout: Duration,
}

impl Default for SummaryWorkerSettings {
    /// The documented defaults.
    fn default() -> Self {
        Self {
            summary_model_id: String::new(),
            summary_system_prompt: crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned(),
            message_content_limit: 4000,
            max_attempts: 3,
            claim_timeout: Duration::from_secs(300),
        }
    }
}

impl SummaryWorkerSettings {
    /// The values of `thread_summary_worker`.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        let worker = &config.thread_summary_worker;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-summary-elsewhere
        // `enabled` and `compression_threshold_pct` belong to the trigger of the message streaming
        // feature, and `reconcile_interval_secs` is deprecated with no effect: none is read here
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-summary-elsewhere
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-summary
        Self {
            summary_model_id: worker.summary_model_id.clone(),
            summary_system_prompt: worker.summary_system_prompt.clone(),
            message_content_limit: worker.message_content_limit,
            max_attempts: worker.max_attempts,
            claim_timeout: Duration::from_secs(worker.claim_timeout_secs),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-summary
    }

    /// The summary model id: the configured one, `gpt-4.1-mini` when it is empty.
    #[must_use]
    pub fn model_id(&self) -> &str {
        if self.summary_model_id.is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            &self.summary_model_id
        }
    }

    /// The bound of the provider phase of one delivery: `claim_timeout` minus the lease margin.
    #[must_use]
    pub fn provider_phase_bound(&self) -> Duration {
        self.claim_timeout.saturating_sub(SUMMARY_LEASE_MARGIN)
    }
}

/// `cleanup_worker` as the chat-cleanup handler reads it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupSettings {
    /// `max_attempts` (greater than 0).
    pub max_attempts: u32,
}

impl Default for CleanupSettings {
    /// The documented default.
    fn default() -> Self {
        Self { max_attempts: 5 }
    }
}

impl CleanupSettings {
    /// The values of `cleanup_worker`.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-cleanup
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-return
        // `enabled`, `poll_interval_secs`, `reconcile_interval_secs`,
        // `stale_in_progress_timeout_secs` and `batch_size` are deprecated with no effect
        // (ADR-0010): only `max_attempts` is read
        Self {
            max_attempts: config.cleanup_worker.max_attempts,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-return
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-cleanup
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod settings_tests;
