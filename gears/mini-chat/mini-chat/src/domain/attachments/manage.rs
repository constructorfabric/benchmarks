//! `GET` and `DELETE` of an attachment (DESIGN 3.3 "Get Attachment", 3.6
//! "Attachment Deletion").
//!
//! Both: PDP (`read_attachment` / `delete_attachment`, one evaluation, the
//! chat as resource), the owner-scoped live chat (404 chat), then the
//! attachment of **this chat, uploaded by the caller** (404 attachment; the
//! uploader check comes before the idempotency check).

use std::sync::Arc;

use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::TxConfig;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{
    AttachmentCleanupEvent, AttachmentCleanupKind, AttachmentDetail, AttachmentRow,
    AttachmentService, SecondaryRef,
};
use crate::domain::authz::{actions, chat_scope};
use crate::domain::chats::chat_not_found;
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::{
    AttachmentCleanupPort, AttachmentLifecycleRepository, ChatRepository, StorageTarget,
};

fn attachment_not_found(id: Uuid) -> DomainError {
    DomainError::NotFound {
        resource: ResourceKind::Attachment,
        id: id.to_string(),
    }
}

impl AttachmentService {
    /// The caller's attachment of the chat, soft-deleted ones included.
    async fn locate(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
        action: &str,
    ) -> Result<(AccessScope, AttachmentRow), DomainError> {
        let owner = chat_scope(&self.enforcer, ctx, action, Some(chat_id)).await?;
        let conn = self.shared.db.conn()?;
        conn.find_live_chat(&owner, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        // The chat is the caller's: the children need the tenant scope only.
        let tenant = owner.tenant_only();
        let row = conn
            .find_attachment(&tenant, chat_id, attachment_id)
            .await?
            .filter(|row| row.uploaded_by_user_id == ctx.subject_id())
            .ok_or_else(|| attachment_not_found(attachment_id))?;
        Ok((tenant, row))
    }

    /// The attachment as the API shows it.
    ///
    /// # Errors
    /// `AccessDenied` / `ServiceUnavailable` (PDP), `NotFound` (chat, or an
    /// attachment that is unknown, deleted, of another chat or uploaded by
    /// another user), `Internal`.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<AttachmentDetail, DomainError> {
        let (_, row) = self
            .locate(ctx, chat_id, attachment_id, actions::READ_ATTACHMENT)
            .await?;
        if row.deleted_at.is_some() {
            return Err(attachment_not_found(attachment_id));
        }
        AttachmentDetail::from_row(&row)
    }

    /// Soft-deletes an attachment that no message references and hands its
    /// provider file to the attachment-cleanup outbox in the same transaction.
    /// A repeated delete is a no-op.
    ///
    /// # Errors
    /// `AccessDenied` / `ServiceUnavailable` (PDP), `NotFound`,
    /// `AttachmentLocked` (a message references it), `Internal` (including a
    /// failing enqueue: the soft delete rolls back).
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<(), DomainError> {
        let (tenant, row) = self
            .locate(ctx, chat_id, attachment_id, actions::DELETE_ATTACHMENT)
            .await?;
        if row.deleted_at.is_some() {
            return Ok(());
        }
        let alias = self.shared.storage.upstream_alias(&StorageTarget {
            tenant_id: row.tenant_id,
            backend: row.storage_backend.clone(),
        });
        let now = now_utc();
        // The first statement of the transaction is the write (see `upload`).
        let wake = self
            .shared
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let (tenant, cleanup, alias) = (
                    tenant.clone(),
                    Arc::clone(&self.shared.cleanup),
                    alias.clone(),
                );
                Box::pin(async move {
                    delete_in_tx(tx, &tenant, chat_id, attachment_id, now, &*cleanup, alias).await
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }
}

/// The delete transaction: the soft delete decides the winner, the link check
/// (after it, so a concurrent send either linked first or finds the row
/// deleted) can still roll it back, and the cleanup message is enqueued last.
async fn delete_in_tx(
    tx: &DbTx<'_>,
    tenant: &AccessScope,
    chat_id: Uuid,
    attachment_id: Uuid,
    now: time::OffsetDateTime,
    cleanup: &dyn AttachmentCleanupPort,
    alias: Option<String>,
) -> Result<Wake, DomainError> {
    if !tx
        .soft_delete_attachment(tenant, attachment_id, now)
        .await?
    {
        // Deleted by a concurrent request: idempotent.
        return Ok(Wake::empty());
    }
    if tx
        .is_attachment_linked(tenant, chat_id, attachment_id)
        .await?
    {
        return Err(DomainError::AttachmentLocked);
    }
    if !tx
        .mark_attachment_cleanup_pending(tenant, attachment_id)
        .await?
    {
        // A cleanup is already owed (a failed row): its message is in flight.
        return Ok(Wake::empty());
    }
    // Read again under the write lock: the provider file id may have been
    // recorded since the first read.
    let row = tx
        .find_attachment(tenant, chat_id, attachment_id)
        .await?
        .ok_or_else(|| attachment_not_found(attachment_id))?;
    let mut event = AttachmentCleanupEvent::for_row(AttachmentCleanupKind::Deleted, &row, now);
    event.secondary_ref = row.secondary_file_id.as_ref().map(|file_id| SecondaryRef {
        file_id: file_id.clone(),
        provider_kind: "anthropic".to_owned(),
        upstream_alias: alias.unwrap_or_else(|| row.storage_backend.clone()),
    });
    cleanup.enqueue_attachment_cleanup(tx, &event).await
}
