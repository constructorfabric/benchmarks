//! Chat cleanup handler (queue `outbox.chat_cleanup_queue_name`).

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use super::{decode, delivery_attempt};
use crate::domain::services::cleanup::CleanupService;
use crate::infra::outbox::payloads::ChatCleanupPayload;

/// Deletes the provider files and the vector store of a soft-deleted chat
/// ([`CleanupService::process_chat_cleanup`]).
pub struct ChatCleanupHandler {
    cleanup: Arc<CleanupService>,
}

impl ChatCleanupHandler {
    #[must_use]
    pub fn new(cleanup: Arc<CleanupService>) -> Self {
        Self { cleanup }
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: ChatCleanupPayload = match decode("chat cleanup", msg) {
            Ok(p) => p,
            Err(reject) => return reject,
        };
        self.cleanup
            .process_chat_cleanup(&payload, delivery_attempt(msg))
            .await
            .into()
    }
}
