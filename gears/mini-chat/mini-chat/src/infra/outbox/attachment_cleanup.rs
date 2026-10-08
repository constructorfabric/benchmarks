//! Attachment-cleanup outbox handler (spec §13.1; DESIGN §4 "Attachment
//! Deletion", Phase 2).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::warn;

use super::payloads::AttachmentCleanupPayload;
use crate::domain::services::cleanup::{CleanupOutcome, CleanupService};

/// Handler of the attachment-cleanup queue.
///
/// A malformed payload is rejected. Otherwise one delivery runs
/// [`CleanupService::cleanup_attachment`]: a failed provider delete is recorded
/// on the attachment and retried (`Retry`) until `cleanup_worker.max_attempts`
/// recorded attempts, which mark the attachment `failed` and dead-letter the
/// message (`Reject`).
pub struct AttachmentCleanupHandler {
    service: Arc<CleanupService>,
}

impl AttachmentCleanupHandler {
    #[must_use]
    pub fn new(service: Arc<CleanupService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    partition = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "rejecting malformed attachment-cleanup outbox payload"
                );
                return MessageResult::Reject(format!("malformed attachment-cleanup payload: {e}"));
            }
        };
        match self.service.cleanup_attachment(&payload).await {
            CleanupOutcome::Done => MessageResult::Ok,
            CleanupOutcome::Retry(_) => MessageResult::Retry,
            CleanupOutcome::Reject(reason) => MessageResult::Reject(reason),
        }
    }
}
