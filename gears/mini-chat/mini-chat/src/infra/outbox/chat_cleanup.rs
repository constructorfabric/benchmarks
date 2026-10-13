//! Handler of the chat-cleanup queue.
//!
//! The message names a soft-deleted chat whose provider resources (the files of its attachments
//! and its vector store) must be removed. The handler validates the message and delegates to the
//! chat cleanup service; the number of deliveries of the message bounds a failing vector store
//! delete.

use std::sync::Arc;

use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use super::cleanup_message_result;
use crate::domain::attachment::ChatCleanup;
use crate::domain::cleanup::ChatCleanupPayload;

/// Leased handler of the chat-cleanup queue.
pub struct ChatCleanupHandler {
    cleanup: Arc<ChatCleanup>,
}

impl ChatCleanupHandler {
    /// Creates the handler.
    #[must_use]
    pub fn new(cleanup: Arc<ChatCleanup>) -> Self {
        Self { cleanup }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(payload) => payload,
            Err(err) => {
                return MessageResult::Reject(format!("malformed chat cleanup payload: {err}"));
            }
        };
        let delivery = u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1);
        cleanup_message_result(self.cleanup.handle(&payload, delivery).await)
    }
}

#[cfg(test)]
#[path = "chat_cleanup_tests.rs"]
mod tests;
