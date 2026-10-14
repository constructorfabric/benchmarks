//! The attachment cleanup enqueue of the shared outbox: one message of the queue
//! `outbox.cleanup_queue_name` inside the caller's transaction, partitioned by `tenant_id`.
//! The payload carries no provider credential; `vector_store_id` is always null and the
//! `secondary_ref` is set only by the `attachment_deleted` event of a row whose secondary copy is
//! `uploaded`.
//!
//! [`AttachmentCleanupHandler`] is the leased handler of the same queue: it maps the outcome of
//! the domain worker to the shared outbox (`Ok`, `Retry`, `Reject`) and replaces the placeholder
//! `UnwiredHandler` of the foundation.

use std::sync::Arc;

use async_trait::async_trait;
use time::format_description::well_known::Rfc3339;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage, Wake};
use toolkit_db::secure::DbTx;

use crate::domain::attachments::cleanup::{AttachmentCleanupWorker, CleanupOutcome};
use crate::domain::attachments::store::{AttachmentCleanupMessage, AttachmentCleanupQueue};
use crate::domain::error::{DomainError, EnqueueSite};
use crate::infra::outbox::{OutboxEnqueuer, QueueKind};

#[async_trait]
impl AttachmentCleanupQueue for OutboxEnqueuer {
    async fn enqueue_attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        message: &AttachmentCleanupMessage,
    ) -> Result<Wake, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-payload
        let deleted_at = message
            .enqueued_at
            .format(&Rfc3339)
            .map_err(|e| DomainError::internal(format!("cannot format the enqueue time: {e}")))?;
        let payload = serde_json::to_vec(&serde_json::json!({
            "event_type": message.event_type.as_str(),
            "tenant_id": message.tenant_id,
            "chat_id": message.chat_id,
            "attachment_id": message.attachment_id,
            "provider_file_id": message.provider_file_id,
            "vector_store_id": serde_json::Value::Null,
            "storage_backend": message.storage_backend,
            "attachment_kind": message.attachment_kind.as_str(),
            "deleted_at": deleted_at,
            "secondary_ref": message.secondary_ref.as_ref().map(|secondary| serde_json::json!({
                "file_id": secondary.file_id,
                "provider_kind": secondary.provider_kind,
                "upstream_alias": secondary.upstream_alias,
            })),
        }))
        .map_err(|e| DomainError::internal(format!("cannot serialize the cleanup: {e}")))?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-payload
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-transactional
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-too-large
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-return-error
        // partitioned by `tenant_id`; a payload above the outbox size limit is returned as the
        // enqueue error and the caller rolls back
        let wake = self
            .enqueue_raw(
                tx,
                QueueKind::AttachmentCleanup,
                message.tenant_id,
                payload,
                "mini_chat.attachment_cleanup",
                EnqueueSite::AttachmentDelete,
            )
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-return-error
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-too-large
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-transactional
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-return
        Ok(wake)
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-enqueue:p1:inst-enq-return
    }
}

/// The leased handler of the attachment cleanup queue.
pub struct AttachmentCleanupHandler {
    worker: Arc<AttachmentCleanupWorker>,
}

impl AttachmentCleanupHandler {
    /// Create the handler over the domain worker.
    #[must_use]
    pub fn new(worker: Arc<AttachmentCleanupWorker>) -> Self {
        Self { worker }
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-return
        // the observed state is the metrics and the dead-letter store of the shared outbox; no
        // admin route is added
        match self.worker.handle(&msg.payload).await {
            CleanupOutcome::Ok => MessageResult::Ok,
            CleanupOutcome::Retry => MessageResult::Retry,
            CleanupOutcome::Reject(reason) => MessageResult::Reject(reason),
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-return
    }
}

#[cfg(test)]
#[path = "attachment_cleanup_tests.rs"]
mod attachment_cleanup_tests;

#[cfg(test)]
#[path = "attachment_enqueue_tests.rs"]
mod attachment_enqueue_tests;
