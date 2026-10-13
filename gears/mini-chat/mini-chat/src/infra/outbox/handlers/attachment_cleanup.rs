//! Attachment cleanup handler (queue `outbox.cleanup_queue_name`).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use super::{decode, delivery_attempt};
use crate::domain::services::cleanup::CleanupService;
use crate::infra::outbox::payloads::AttachmentCleanupPayload;

/// Deletes the provider file of a deleted / abandoned / failed attachment
/// ([`CleanupService::process_attachment_cleanup`]).
pub struct AttachmentCleanupHandler {
    cleanup: Arc<CleanupService>,
}

impl AttachmentCleanupHandler {
    #[must_use]
    pub fn new(cleanup: Arc<CleanupService>) -> Self {
        Self { cleanup }
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: AttachmentCleanupPayload = match decode("attachment cleanup", msg) {
            Ok(p) => p,
            Err(reject) => return reject,
        };
        self.cleanup
            .process_attachment_cleanup(&payload, delivery_attempt(msg))
            .await
            .into()
    }
}
