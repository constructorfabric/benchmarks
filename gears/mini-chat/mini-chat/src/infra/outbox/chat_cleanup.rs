//! Chat-cleanup outbox message produced by the chat soft delete
//! (`cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`, steps 7 to 10) and the
//! handler that consumes it and replaces the foundation placeholder
//! (`cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxError, OutboxMessage};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::workers::chat_cleanup::ChatCleanupService;
use crate::infra::outbox::thread_summary::message_result;

/// Value of `reason` for a chat soft delete.
pub const CHAT_CLEANUP_REASON: &str = "chat_soft_delete";

/// Serialized chat-cleanup message: exactly these five keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatCleanupPayload {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The soft-deleted chat.
    pub chat_id: Uuid,
    /// Server-generated UUID v4, fixed for every retry of the message.
    pub system_request_id: Uuid,
    /// Always [`CHAT_CLEANUP_REASON`].
    pub reason: String,
    /// The deletion time (`chats.deleted_at`), RFC 3339.
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

impl ChatCleanupPayload {
    /// Builds the payload with a fresh `system_request_id`.
    #[must_use]
    pub fn new(tenant_id: Uuid, chat_id: Uuid, chat_deleted_at: OffsetDateTime) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-build-cleanup-payload
        Self {
            tenant_id,
            chat_id,
            system_request_id: Uuid::new_v4(),
            reason: CHAT_CLEANUP_REASON.to_owned(),
            chat_deleted_at,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-build-cleanup-payload
    }

    /// The JSON bytes enqueued on the chat-cleanup queue.
    ///
    /// # Errors
    ///
    /// Serialization failure (not expected for this type).
    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

/// A payload that cannot be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed chat cleanup payload: {0}")]
pub struct MalformedChatCleanup(pub String);

/// Decodes the payload for the handler.
///
/// # Errors
///
/// [`MalformedChatCleanup`] for a missing or unknown key, an id that is not a
/// UUID, a `chat_deleted_at` that is not RFC 3339, a `reason` other than
/// `chat_soft_delete`, or not JSON at all.
pub fn decode_chat_cleanup(bytes: &[u8]) -> Result<ChatCleanupPayload, MalformedChatCleanup> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-decode-chat-cleanup
    let payload: ChatCleanupPayload =
        serde_json::from_slice(bytes).map_err(|error| MalformedChatCleanup(error.to_string()))?;
    if payload.reason != CHAT_CLEANUP_REASON {
        return Err(MalformedChatCleanup(format!("reason `{}`", payload.reason)));
    }
    Ok(payload)
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup:p1:inst-decode-chat-cleanup
}

/// The handler of the chat cleanup queue.
pub struct ChatCleanupHandler {
    service: ChatCleanupService,
}

impl ChatCleanupHandler {
    /// Creates the handler over the cleanup logic.
    #[must_use]
    pub fn new(service: ChatCleanupService) -> Self {
        Self { service }
    }
}

// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-chat-cleanup-handler:p1
#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-deliver-chat-cleanup
        let payload = match decode_chat_cleanup(&msg.payload) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(partition = msg.partition_id, %error, "a malformed chat cleanup payload is rejected");
                return MessageResult::Reject(error.to_string());
            }
        };
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-deliver-chat-cleanup
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-run-chat-cleanup
        let outcome = self
            .service
            .handle(payload.tenant_id, payload.chat_id, msg.attempts)
            .await;
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-run-chat-cleanup
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-match-chat-cleanup-result
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-case-cleanup-ok
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-case-cleanup-retry
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-case-cleanup-reject
        let result = message_result(outcome);
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-case-cleanup-reject
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-case-cleanup-retry
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-case-cleanup-ok
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-match-chat-cleanup-result
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-return-chat-cleanup
        result
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-chat-deletion-cleanup:p1:inst-return-chat-cleanup
    }
}

/// Maps an outbox enqueue failure of the chat-cleanup message to the domain
/// error: an oversized payload is [`DomainError::CleanupPayloadTooLarge`] (its
/// `detail` carries the outbox message), any other failure is internal.
#[must_use]
pub fn map_enqueue_error(error: &OutboxError) -> DomainError {
    match error {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-catch-payload-too-large
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-return-payload-too-large
        OutboxError::PayloadTooLarge { size, max } => DomainError::CleanupPayloadTooLarge {
            detail: format!(
                "chat cleanup payload of {size} bytes exceeds the limit of {max} bytes"
            ),
        },
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-return-payload-too-large
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-catch-payload-too-large
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-catch-delete-failure
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-return-delete-internal
        other => DomainError::Internal(format!("chat cleanup enqueue failed: {other}")),
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-return-delete-internal
        // @cpt-end:cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat:p1:inst-catch-delete-failure
    }
}

#[cfg(test)]
#[path = "chat_cleanup_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "chat_cleanup_handler_tests.rs"]
mod handler_tests;

#[cfg(test)]
#[path = "chat_cleanup_budget_tests.rs"]
mod budget_tests;

#[cfg(test)]
#[path = "chat_cleanup_startup_tests.rs"]
mod startup_tests;
