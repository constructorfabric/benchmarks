//! Handler of the attachment-cleanup queue (DESIGN "Attachment Deletion" phase 2).

use std::sync::Arc;

use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use super::cleanup_message_result;
use crate::domain::attachment::AttachmentCleanup;
use crate::domain::cleanup::AttachmentCleanupPayload;

/// Leased handler of the attachment-cleanup queue: a malformed payload is rejected at once, every
/// other outcome comes from the cleanup service.
pub struct AttachmentCleanupHandler {
    cleanup: Arc<AttachmentCleanup>,
}

impl AttachmentCleanupHandler {
    /// Creates the handler.
    #[must_use]
    pub fn new(cleanup: Arc<AttachmentCleanup>) -> Self {
        Self { cleanup }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(payload) => payload,
            Err(err) => {
                return MessageResult::Reject(format!(
                    "malformed attachment cleanup payload: {err}"
                ));
            }
        };
        cleanup_message_result(self.cleanup.handle(&payload).await)
    }
}
