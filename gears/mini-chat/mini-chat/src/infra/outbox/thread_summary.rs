//! Thread summary handler (DESIGN section 3.6 "Thread Summary Update"):
//! deserializes the `mini-chat.thread_summary` payload and runs
//! [`ThreadSummaryService`].

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use super::payloads::ThreadSummaryPayload;
use crate::domain::services::thread_summary_service::{SummaryRunResult, ThreadSummaryService};

/// `mini-chat.thread_summary` handler: `Done` -> `Ok`, `Retry` -> `Retry`,
/// `Reject` (and a malformed payload) -> `Reject`.
pub struct ThreadSummaryHandler {
    service: Arc<ThreadSummaryService>,
}

impl ThreadSummaryHandler {
    #[must_use]
    pub fn new(service: Arc<ThreadSummaryService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(seq = msg.seq, error = %e, "malformed thread summary payload");
                return MessageResult::Reject(format!("malformed thread summary payload: {e}"));
            }
        };
        match self.service.run(&p, msg.attempts).await {
            SummaryRunResult::Done => MessageResult::Ok,
            SummaryRunResult::Retry(reason) => {
                tracing::warn!(chat_id = %p.chat_id, attempts = msg.attempts, %reason, "thread summary: retrying");
                MessageResult::Retry
            }
            SummaryRunResult::Reject(reason) => MessageResult::Reject(reason),
        }
    }
}
