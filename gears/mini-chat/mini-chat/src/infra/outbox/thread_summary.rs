//! Thread-summary outbox handler (spec §14; DESIGN §3.6 "Thread Summary
//! Update", "Asynchronous execution").

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::warn;

use super::payloads::ThreadSummaryPayload;
use crate::domain::services::thread_summary::{SummaryOutcome, ThreadSummaryService};

/// Handler of the thread-summary queue.
///
/// Deserializes the payload (malformed -> `Reject`) and runs one attempt of the
/// task. A `Retry` on the `thread_summary_worker.max_attempts`-th delivery
/// becomes `Reject` (`OutboxMessage::attempts` is 0-based), so a persistent
/// failure does not block the other chats of the partition.
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
        let payload: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    partition = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "rejecting malformed thread-summary outbox payload"
                );
                return MessageResult::Reject(format!("malformed thread-summary payload: {e}"));
            }
        };
        match self.service.run(&payload).await {
            SummaryOutcome::Done => MessageResult::Ok,
            SummaryOutcome::Reject(reason) => MessageResult::Reject(reason),
            SummaryOutcome::Retry(reason) => {
                let delivery = i64::from(msg.attempts) + 1;
                if delivery >= i64::from(self.service.max_attempts()) {
                    warn!(chat_id = %payload.chat_id, system_request_id = %payload.system_request_id, delivery, %reason, "thread summary attempts exhausted; dead-lettering");
                    MessageResult::Reject(format!(
                        "thread summary failed after {delivery} deliveries: {reason}"
                    ))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}
