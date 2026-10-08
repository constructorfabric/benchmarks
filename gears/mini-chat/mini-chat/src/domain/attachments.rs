//! Attachments: metadata, deletion and upload (DESIGN §3.3, §3.6 "File Upload").

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::app::AppServices;
use crate::domain::authz::actions;
use crate::domain::error::{DomainError, DomainResult, Resource, retry_contention};
use crate::domain::time::now;
use crate::infra::db::entities::attachment;
use crate::infra::db::repo;
use crate::infra::outbox::{AttachmentCleanupPayload, Wakes};

impl AppServices {
    /// `GET /chats/{id}/attachments/{attachment_id}`.
    ///
    /// # Errors
    /// `NotFound(Chat|Attachment)`.
    pub async fn get_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> DomainResult<attachment::Model> {
        let chat = self
            .authorized_chat(ctx, actions::READ_ATTACHMENT, chat_id)
            .await?;
        let conn = self.db.conn()?;
        repo::find_attachment_any(&conn, chat.tenant_id, chat.id, attachment_id)
            .await?
            .filter(|a| a.deleted_at.is_none() && a.uploaded_by_user_id == ctx.subject_id())
            .ok_or(DomainError::NotFound(Resource::Attachment))
    }

    /// `DELETE /chats/{id}/attachments/{attachment_id}`: soft delete + outbox cleanup.
    ///
    /// # Errors
    /// `NotFound`, `AttachmentLocked`.
    pub async fn delete_attachment(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> DomainResult<()> {
        let chat = self
            .authorized_chat(ctx, actions::DELETE_ATTACHMENT, chat_id)
            .await?;
        let conn = self.db.conn()?;
        let att = repo::find_attachment_any(&conn, chat.tenant_id, chat.id, attachment_id)
            .await?
            .filter(|a| a.uploaded_by_user_id == ctx.subject_id())
            .ok_or(DomainError::NotFound(Resource::Attachment))?;
        if att.deleted_at.is_some() {
            return Ok(());
        }
        if repo::attachment_referenced(&conn, chat.tenant_id, chat.id, att.id).await? {
            return Err(DomainError::AttachmentLocked);
        }
        let outbox = std::sync::Arc::clone(&self.outbox);
        let wakes = retry_contention(|| {
            let outbox = std::sync::Arc::clone(&outbox);
            let att = att.clone();
            self.db.transaction(move |tx| {
                Box::pin(async move {
                    let ts = now();
                    let n = repo::update_attachment_where(
                        tx,
                        att.tenant_id,
                        att.id,
                        Condition::all().add(attachment::Column::DeletedAt.is_null()),
                        vec![
                            (attachment::Column::DeletedAt, Expr::value(ts)),
                            (attachment::Column::UpdatedAt, Expr::value(ts)),
                            (attachment::Column::CleanupStatus, Expr::value("pending")),
                            (attachment::Column::CleanupUpdatedAt, Expr::value(ts)),
                        ],
                    )
                    .await?;
                    let mut wakes = Wakes::default();
                    if n == 0 {
                        return Ok(wakes);
                    }
                    let payload = AttachmentCleanupPayload {
                        event_type: "attachment_deleted".to_owned(),
                        tenant_id: att.tenant_id,
                        chat_id: att.chat_id,
                        attachment_id: att.id,
                        provider_file_id: att.provider_file_id.clone(),
                        vector_store_id: None,
                        storage_backend: att.storage_backend.clone(),
                        attachment_kind: att.attachment_kind.clone(),
                        deleted_at: ts,
                        secondary_ref: None,
                    };
                    wakes.push(
                        outbox
                            .attachment_cleanup(tx, &payload)
                            .await
                            .map_err(crate::domain::turns::internal_payload)?,
                    );
                    Ok(wakes)
                })
            })
        })
        .await?;
        wakes.fire();
        Ok(())
    }
}
