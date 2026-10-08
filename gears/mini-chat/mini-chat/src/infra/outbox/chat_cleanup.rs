//! Chat-cleanup outbox handler (spec §13.1; DESIGN §3.6 "Cleanup on Chat
//! Deletion").

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::warn;

use super::payloads::ChatCleanupPayload;
use crate::domain::services::cleanup::{CleanupOutcome, CleanupService};

/// Handler of the chat-cleanup queue.
///
/// A malformed payload and a chat that is not soft-deleted are rejected. One
/// delivery runs [`CleanupService::cleanup_chat`]; `OutboxMessage::attempts` is
/// 0-based, so the delivery that reaches `cleanup_worker.max_attempts` is
/// `attempts + 1`, and every delivery counts toward the limit.
pub struct ChatCleanupHandler {
    service: Arc<CleanupService>,
}

impl ChatCleanupHandler {
    #[must_use]
    pub fn new(service: Arc<CleanupService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    partition = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "rejecting malformed chat-cleanup outbox payload"
                );
                return MessageResult::Reject(format!("malformed chat-cleanup payload: {e}"));
            }
        };
        let delivery = u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1);
        match self.service.cleanup_chat(&payload, delivery).await {
            CleanupOutcome::Done => MessageResult::Ok,
            CleanupOutcome::Retry(_) => MessageResult::Retry,
            CleanupOutcome::Reject(reason) => MessageResult::Reject(reason),
        }
    }
}
