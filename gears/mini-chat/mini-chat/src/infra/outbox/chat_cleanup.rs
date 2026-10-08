//! Chat cleanup handler (DESIGN section 3.6 "Cleanup on Chat Deletion"):
//! provider files of every `pending` attachment of a soft-deleted chat, then
//! the chat's vector store once no attachment is `pending`.

use async_trait::async_trait;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use uuid::Uuid;

use super::attachment_cleanup::{
    CleanupDeps, DeleteFailure, FileCleanup, FileTarget, classify_delete,
};
use super::payloads::ChatCleanupPayload;
use crate::domain::error::DomainError;
use crate::infra::db::entities::chat_vector_store;
use crate::infra::db::repos::{attachment_repo, chat_repo, tenant_scope, vector_store_repo};

/// `mini-chat.chat_cleanup` handler.
pub struct ChatCleanupHandler {
    deps: CleanupDeps,
}

impl ChatCleanupHandler {
    #[must_use]
    pub fn new(deps: CleanupDeps) -> Self {
        Self { deps }
    }

    async fn process(
        &self,
        p: &ChatCleanupPayload,
        attempts: i16,
    ) -> Result<MessageResult, DomainError> {
        let conn = self.deps.db.conn()?;
        let Some(chat) =
            chat_repo::find_any_scoped(&conn, &tenant_scope(p.tenant_id), p.chat_id).await?
        else {
            return Ok(MessageResult::Reject("chat not found".to_owned()));
        };
        if chat.deleted_at.is_none() {
            return Ok(MessageResult::Reject("chat is not soft-deleted".to_owned()));
        }

        let pending =
            attachment_repo::pending_cleanup_in_chat(&conn, p.tenant_id, p.chat_id).await?;
        let mut left = 0usize;
        for row in &pending {
            let target = FileTarget {
                tenant_id: row.tenant_id,
                chat_id: row.chat_id,
                attachment_id: row.id,
                provider_file_id: row.provider_file_id.as_deref(),
                storage_backend: &row.storage_backend,
                seen_attempts: row.cleanup_attempts,
                secondary_kind: row
                    .secondary_file_id
                    .as_ref()
                    .and(row.secondary_provider_kind.as_deref()),
            };
            match self.deps.clean_file(&target).await? {
                FileCleanup::Done | FileCleanup::Failed => {}
                FileCleanup::Retry | FileCleanup::Deferred => left += 1,
            }
        }
        if left > 0 {
            tracing::info!(chat_id = %p.chat_id, left, "chat cleanup: attachments still pending; retrying");
            return Ok(MessageResult::Retry);
        }

        match vector_store_repo::find(&conn, p.tenant_id, p.chat_id).await? {
            None => Ok(MessageResult::Ok),
            Some(row) => self.delete_vector_store(p, &row, attempts).await,
        }
    }

    /// Every attachment is terminal: delete the provider store (404 is
    /// success), then the row (completion marker). A failed delete keeps the
    /// row and retries until the delivery that reaches `max_attempts`.
    pub(super) async fn delete_vector_store(
        &self,
        p: &ChatCleanupPayload,
        row: &chat_vector_store::Model,
        attempts: i16,
    ) -> Result<MessageResult, DomainError> {
        let Some(vs) = row.vector_store_id.as_deref() else {
            return self.delete_placeholder(p, row).await;
        };
        let conn = self.deps.db.conn()?;
        if attachment_repo::has_failed_cleanup(&conn, p.tenant_id, p.chat_id).await? {
            tracing::warn!(
                chat_id = %p.chat_id,
                "deleting the vector store of a chat with failed attachment cleanups"
            );
        }
        match self
            .delete_provider_store(p.tenant_id, &row.provider, vs)
            .await
        {
            Ok(()) => {
                vector_store_repo::delete_row(&conn, p.tenant_id, p.chat_id, row.id).await?;
                tracing::debug!(chat_id = %p.chat_id, "chat vector store cleanup done");
                Ok(MessageResult::Ok)
            }
            Err(failure) => Ok(self.store_delete_failed(p, attempts, failure)),
        }
    }

    /// Creation still in flight (or its creator died): no provider store to
    /// delete, so the placeholder row goes. A live creator then loses its CAS
    /// and deletes the store it created. If its CAS won since our read (0
    /// rows), the row now names a real store: `Retry`, and the next delivery
    /// deletes it.
    async fn delete_placeholder(
        &self,
        p: &ChatCleanupPayload,
        row: &chat_vector_store::Model,
    ) -> Result<MessageResult, DomainError> {
        let conn = self.deps.db.conn()?;
        let n =
            vector_store_repo::delete_placeholder(&conn, p.tenant_id, p.chat_id, row.id).await?;
        if n == 0 {
            tracing::info!(chat_id = %p.chat_id, "vector store placeholder replaced meanwhile; retrying");
            return Ok(MessageResult::Retry);
        }
        Ok(MessageResult::Ok)
    }

    /// Provider delete through the store's backend; 2xx and 404 are success.
    async fn delete_provider_store(
        &self,
        tenant_id: Uuid,
        backend: &str,
        vs: &str,
    ) -> Result<(), DeleteFailure> {
        let st = self
            .deps
            .resolver
            .storage_for_backend_label(backend, tenant_id)
            .map_err(|e| DeleteFailure::Attempt(e.to_string()))?;
        classify_delete(self.deps.storage.delete_vector_store(&st, vs).await)
    }

    /// A failed attempt: `Retry`, or `Reject` on the delivery that reaches
    /// `max_attempts`. A request that was never sent is infrastructure:
    /// `Retry` without the delivery limit.
    fn store_delete_failed(
        &self,
        p: &ChatCleanupPayload,
        attempts: i16,
        failure: DeleteFailure,
    ) -> MessageResult {
        let max = self.deps.max_attempts;
        let last = i64::from(attempts) + 1 >= i64::from(max);
        // A request that was never sent does not count toward the limit.
        let (result, error) = match failure {
            DeleteFailure::Attempt(error) if last => (
                MessageResult::Reject(format!("vector store delete: max attempts ({max}) reached")),
                error,
            ),
            DeleteFailure::NotSent(error) | DeleteFailure::Attempt(error) => {
                (MessageResult::Retry, error)
            }
        };
        tracing::warn!(chat_id = %p.chat_id, attempts, %error, outcome = ?result, "vector store delete failed");
        result
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(seq = msg.seq, error = %e, "malformed chat cleanup payload");
                return MessageResult::Reject(format!("malformed chat cleanup payload: {e}"));
            }
        };
        match self.process(&p, msg.attempts).await {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!(chat_id = %p.chat_id, error = %e, "chat cleanup: database failure; retrying");
                MessageResult::Retry
            }
        }
    }
}
