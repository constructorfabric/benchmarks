//! Leased outbox handlers of the five mini-chat queues (S§10.1, D§5.6
//! "Shared Outbox Processing Model").
//!
//! Every handler deserializes the payload first: a malformed payload is
//! `Reject` (dead letter). Retry/backoff, leases and dead-lettering are the
//! shared outbox's; the handlers only map outcomes.

use serde::de::DeserializeOwned;
use toolkit_db::outbox::{MessageResult, OutboxMessage};
use tracing::warn;

use crate::domain::ports::HandlerOutcome;

mod attachment_cleanup;
mod audit;
mod chat_cleanup;
mod thread_summary;
mod usage;

pub use attachment_cleanup::AttachmentCleanupHandler;
pub use audit::{AUDIT_MAX_ATTEMPTS, AUDIT_PLUGIN_TIMEOUT, AuditCounts, AuditHandler};
pub use chat_cleanup::ChatCleanupHandler;
pub use thread_summary::ThreadSummaryHandler;
pub use usage::UsageHandler;

/// 1-based delivery number of `msg` (`attempts` counts earlier deliveries).
#[must_use]
pub fn delivery_attempt(msg: &OutboxMessage) -> u32 {
    u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1)
}

/// Deserialize the JSON payload of a `queue` message; malformed → `Reject`.
fn decode<T: DeserializeOwned>(queue: &str, msg: &OutboxMessage) -> Result<T, MessageResult> {
    serde_json::from_slice(&msg.payload).map_err(|e| {
        warn!(queue, partition_id = msg.partition_id, seq = msg.seq, error = %e, "malformed outbox payload");
        MessageResult::Reject(format!("malformed {queue} payload: {e}"))
    })
}

impl From<HandlerOutcome> for MessageResult {
    fn from(o: HandlerOutcome) -> Self {
        match o {
            HandlerOutcome::Ok => Self::Ok,
            HandlerOutcome::Retry => Self::Retry,
            HandlerOutcome::Reject(reason) => Self::Reject(reason),
        }
    }
}

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;
