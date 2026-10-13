//! Handler of the thread-summary queue (DESIGN section 3.6 "Thread Summary Update").

use std::sync::Arc;

use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::warn;

use crate::domain::summary::{SummaryOutcome, SummaryService, SummaryWorkItem};

/// Leased handler of the thread-summary queue.
///
/// A malformed payload is rejected. A summary attempt that would be retried on its
/// `thread_summary_worker.max_attempts`-th delivery is rejected instead, so a persistent failure
/// is dead-lettered and does not block the other chats of the partition.
pub struct ThreadSummaryHandler {
    service: Arc<SummaryService>,
    max_attempts: u32,
}

impl ThreadSummaryHandler {
    /// Creates the handler.
    #[must_use]
    pub fn new(service: Arc<SummaryService>, max_attempts: u32) -> Self {
        Self {
            service,
            max_attempts,
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let item: SummaryWorkItem = match serde_json::from_slice(&msg.payload) {
            Ok(item) => item,
            Err(err) => {
                return MessageResult::Reject(format!("malformed thread summary payload: {err}"));
            }
        };
        match self.service.run(&item).await {
            SummaryOutcome::Done => MessageResult::Ok,
            SummaryOutcome::Reject(cause) => MessageResult::Reject(cause),
            SummaryOutcome::Retry => {
                let delivery = u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1);
                if delivery >= self.max_attempts {
                    warn!(
                        chat_id = %item.chat_id,
                        system_request_id = %item.system_request_id,
                        "the thread summary task failed on its last attempt; dead-lettering it"
                    );
                    MessageResult::Reject(format!(
                        "the summary failed: max attempts ({}) reached",
                        self.max_attempts
                    ))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}
