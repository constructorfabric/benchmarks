//! `mini-chat.attachment_cleanup` handler: deletes the provider file of a deleted, abandoned or
//! failed-indexing attachment (DESIGN §4 "Attachment Deletion" phase 2, "Attachment cleanup
//! state machine").

use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::clock;
use crate::domain::attachments::{self, CLEANUP_DONE, CLEANUP_FAILED, CLEANUP_PENDING, system_ctx};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::{attachment, chat};
use crate::infra::llm::storage;
use crate::infra::outbox::payloads::AttachmentCleanupPayload;

/// Result of one provider-file cleanup attempt of an attachment row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCleanup {
    /// Deleted, already gone (404), no file, or the row is no longer `pending`.
    Done,
    /// Failed attempt below the limit; still `pending`.
    Pending(String),
    /// Failed attempt that reached `cleanup_worker.max_attempts`; now `failed`.
    Failed(String),
}

/// Deletes the provider file of a `pending` cleanup row and records the outcome
/// (`done`, or an attempt with `last_cleanup_error`; terminal `failed` at the limit).
///
/// # Errors
/// DB errors (no attempt is counted).
pub async fn cleanup_attachment_file(
    app: &AppServices,
    row: &attachment::Model,
    fallback_file_id: Option<String>,
) -> Result<FileCleanup, DomainError> {
    let outcome = match row.provider_file_id.clone().or(fallback_file_id) {
        None => Ok(()),
        Some(file_id) => match app.providers.resolve_storage_backend(&row.storage_backend, row.tenant_id) {
            Err(e) => Err(e.to_string()),
            Ok(provider) => {
                let ctx = system_ctx(row.tenant_id);
                storage::delete_file(app.transport.as_ref(), &ctx, &provider, &file_id)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            }
        },
    };
    let scope = AccessScope::for_tenant(row.tenant_id);
    let pending_row = Condition::all()
        .add(attachment::Column::Id.eq(row.id))
        .add(attachment::Column::CleanupStatus.eq(CLEANUP_PENDING));
    let conn = app.db.conn()?;
    let now = clock::now();
    match outcome {
        Ok(()) => {
            attachment::Entity::update_many()
                .col_expr(attachment::Column::CleanupStatus, Expr::value(Some(CLEANUP_DONE.to_owned())))
                .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                .filter(pending_row)
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await?;
            Ok(FileCleanup::Done)
        }
        Err(err) => {
            let max = i32::try_from(app.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
            let attempts = row.cleanup_attempts.saturating_add(1);
            let terminal = attempts >= max;
            let mut q = attachment::Entity::update_many()
                .col_expr(attachment::Column::CleanupAttempts, Expr::value(attempts))
                .col_expr(attachment::Column::LastCleanupError, Expr::value(Some(err.clone())))
                .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)));
            if terminal {
                q = q.col_expr(attachment::Column::CleanupStatus, Expr::value(Some(CLEANUP_FAILED.to_owned())));
            }
            q.filter(pending_row).secure().scope_with(&scope).exec(&conn).await?;
            tracing::warn!(attachment_id = %row.id, attempts, terminal, error = %err, "provider file delete failed");
            Ok(if terminal { FileCleanup::Failed(err) } else { FileCleanup::Pending(err) })
        }
    }
}

/// Loads a chat by id regardless of its deletion state (background work).
///
/// # Errors
/// DB errors.
pub async fn find_chat_any(app: &AppServices, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<chat::Model>, DomainError> {
    let conn = app.db.conn()?;
    Ok(chat::Entity::find()
        .filter(chat::Column::Id.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .one(&conn)
        .await?)
}

pub struct AttachmentCleanupHandler {
    app: Arc<AppServices>,
}

impl AttachmentCleanupHandler {
    #[must_use]
    pub fn new(app: Arc<AppServices>) -> Self {
        Self { app }
    }

    async fn process(&self, p: &AttachmentCleanupPayload) -> Result<MessageResult, DomainError> {
        let app = &self.app;
        match find_chat_any(app, p.tenant_id, p.chat_id).await? {
            None => return Ok(MessageResult::Ok),
            // Chat-deletion cleanup owns the files of a deleted chat.
            Some(c) if c.deleted_at.is_some() => return Ok(MessageResult::Ok),
            Some(_) => {}
        }
        let Some(row) = attachments::find_by_id(app, p.tenant_id, p.attachment_id).await? else {
            return Ok(MessageResult::Ok);
        };
        if row.chat_id != p.chat_id || row.cleanup_status.as_deref() != Some(CLEANUP_PENDING) {
            return Ok(MessageResult::Ok);
        }
        if let Some(sec) = &p.secondary_ref {
            tracing::warn!(attachment_id = %row.id, file_id = %sec.file_id, "secondary file cleanup is not supported; skipped");
        }
        Ok(match cleanup_attachment_file(app, &row, p.provider_file_id.clone()).await? {
            FileCleanup::Done => MessageResult::Ok,
            FileCleanup::Pending(_) => MessageResult::Retry,
            FileCleanup::Failed(e) => MessageResult::Reject(format!(
                "attachment cleanup: max attempts ({}) reached: {e}",
                app.cfg.cleanup_worker.max_attempts
            )),
        })
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}")),
        };
        match self.process(&payload).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(attachment_id = %payload.attachment_id, error = %e, "attachment cleanup: infrastructure error, retrying");
                MessageResult::Retry
            }
        }
    }
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod tests;
