//! Background workers of the gear (`cpt-cf-mini-chat-feature-background-workers`):
//! the monotonic clock of the deadlines, the ports of the workers, the chat
//! cleanup service, the orphan finalization inputs and the thread-summary
//! trigger, prompt, parsing and handler.

// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-deferred:p1
// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-quality-gates:p1
pub mod chat_cleanup;
pub mod clock;
pub mod metrics;
pub mod orphan;
pub mod ports;
pub mod summary;
pub mod summary_parse;
pub mod summary_prompt;
pub mod summary_trigger;

#[cfg(test)]
pub(crate) mod test_support;

use std::time::Duration;

/// `CHAT_CLEANUP_DELIVERY_BUDGET`: time after the delivery start during which
/// the chat cleanup handler starts provider calls.
pub const CHAT_CLEANUP_DELIVERY_BUDGET: Duration = Duration::from_secs(25);

/// `CHAT_CLEANUP_CONCURRENCY`: attachments of one chat processed concurrently.
pub const CHAT_CLEANUP_CONCURRENCY: usize = 4;

/// `SUMMARY_COMMIT_RESERVE`: time kept free before the end of the
/// thread-summary lease for the commit.
pub const SUMMARY_COMMIT_RESERVE: Duration = Duration::from_secs(10);

/// `SUMMARY_RESPONSE_MAX_BYTES`: maximum summary response body read.
pub const SUMMARY_RESPONSE_MAX_BYTES: usize = 1024 * 1024;

/// `ORPHAN_SCAN_BATCH`: candidates per watchdog scan.
pub const ORPHAN_SCAN_BATCH: u32 = 100;

/// Bound of one model-policy call of a worker (`POLICY_PLUGIN_CALL_TIMEOUT`).
pub const POLICY_CALL_BOUND: Duration = Duration::from_secs(10);

/// The system subject of the provider `user` value of a system task
/// (DESIGN "Provider Request Metadata").
pub const SYSTEM_SUBJECT: uuid::Uuid = uuid::uuid!("11111111-6a88-4768-9dfc-6bcd5187d9ed");

/// What a worker handler tells the shared outbox.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    /// Acknowledge the message.
    Ok,
    /// Deliver it again later.
    Retry,
    /// Dead-letter it with the reason.
    Reject(String),
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
