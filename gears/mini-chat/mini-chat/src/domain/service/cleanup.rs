//! Attachment and chat cleanup outbox handlers (OWNER: attachments).
//!
//! Provider deletes are idempotent: 2xx and 404 are success. Per-attachment attempts are
//! tracked in `attachments.cleanup_*`; the shared outbox owns retries, backoff and dead letters.

use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::outbox_payloads::{AttachmentCleanupEvent, ChatCleanupEvent};
use crate::domain::service::Deps;
use crate::domain::service::attachments::cleanup_status;
use crate::infra::db::entity::{attachment, chat, chat_vector_store};
use crate::infra::llm::StorageError;

/// Result of one provider file delete attempt.
enum FileDelete {
    Done,
    Failed(String),
}

/// Deletes a provider file through the provider mapped from `storage_backend`.
async fn delete_provider_file(
    deps: &Deps,
    tenant_id: Uuid,
    storage_backend: &str,
    file_id: &str,
) -> FileDelete {
    let Some(provider_id) = deps.providers.provider_for_storage_backend(storage_backend) else {
        return FileDelete::Failed(format!("unknown storage backend '{storage_backend}'"));
    };
    match deps.storage.delete_file(&provider_id, tenant_id, file_id).await {
        Ok(()) => FileDelete::Done,
        Err(e) if e.is_not_found() => FileDelete::Done,
        Err(e) => FileDelete::Failed(e.to_string()),
    }
}

async fn load_chat_any(deps: &Deps, tenant_id: Uuid, chat_id: Uuid) -> Result<Option<chat::Model>, DomainError> {
    let conn = deps.db.conn()?;
    Ok(chat::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .and_id(chat_id)?
        .one(&conn)
        .await?)
}

async fn mark_done(deps: &Deps, tenant_id: Uuid, chat_id: Uuid, id: Uuid) -> Result<(), DomainError> {
    let conn = deps.db.conn()?;
    attachment::Entity::update_many()
        .col_expr(attachment::Column::CleanupStatus, Expr::value(cleanup_status::DONE))
        .col_expr(
            attachment::Column::CleanupUpdatedAt,
            Expr::value(Some(OffsetDateTime::now_utc())),
        )
        .filter(
            Condition::all()
                .add(attachment::Column::Id.eq(id))
                .add(attachment::Column::ChatId.eq(chat_id)),
        )
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(&conn)
        .await?;
    Ok(())
}

/// Records a failed attempt; returns true when the attachment became terminal `failed`.
async fn record_failure(
    deps: &Deps,
    row: &attachment::Model,
    error: &str,
) -> Result<bool, DomainError> {
    let attempts = row.cleanup_attempts.saturating_add(1);
    let max = i32::try_from(deps.cfg.cleanup_worker.max_attempts.max(1)).unwrap_or(i32::MAX);
    let terminal = attempts >= max;
    let mut q = attachment::Entity::update_many()
        .col_expr(attachment::Column::CleanupAttempts, Expr::value(attempts))
        .col_expr(attachment::Column::LastCleanupError, Expr::value(Some(error.to_owned())))
        .col_expr(
            attachment::Column::CleanupUpdatedAt,
            Expr::value(Some(OffsetDateTime::now_utc())),
        );
    if terminal {
        q = q.col_expr(attachment::Column::CleanupStatus, Expr::value(cleanup_status::FAILED));
    }
    let conn = deps.db.conn()?;
    q.filter(
        Condition::all()
            .add(attachment::Column::Id.eq(row.id))
            .add(attachment::Column::ChatId.eq(row.chat_id)),
    )
    .secure()
    .scope_with(&AccessScope::for_tenant(row.tenant_id))
    .exec(&conn)
    .await?;
    Ok(terminal)
}

/// Attachment cleanup queue handler.
pub struct AttachmentCleanupHandler {
    deps: Arc<Deps>,
}

impl AttachmentCleanupHandler {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    async fn process(&self, ev: &AttachmentCleanupEvent) -> Result<MessageResult, DomainError> {
        let deps = &*self.deps;
        // The chat cleanup path owns the files of soft-deleted chats.
        match load_chat_any(deps, ev.tenant_id, ev.chat_id).await? {
            Some(c) if c.deleted_at.is_none() => {}
            _ => return Ok(MessageResult::Ok),
        }
        let row = {
            let conn = deps.db.conn()?;
            attachment::Entity::find()
                .filter(attachment::Column::ChatId.eq(ev.chat_id))
                .secure()
                .scope_with(&AccessScope::for_tenant(ev.tenant_id))
                .and_id(ev.attachment_id)?
                .one(&conn)
                .await?
        };
        let Some(row) = row else {
            return Ok(MessageResult::Ok);
        };
        if matches!(
            row.cleanup_status.as_deref(),
            Some(cleanup_status::DONE | cleanup_status::FAILED)
        ) {
            return Ok(MessageResult::Ok);
        }
        let Some(file_id) = ev.provider_file_id.clone().or_else(|| row.provider_file_id.clone()) else {
            mark_done(deps, row.tenant_id, row.chat_id, row.id).await?;
            return Ok(MessageResult::Ok);
        };
        match delete_provider_file(deps, ev.tenant_id, &ev.storage_backend, &file_id).await {
            FileDelete::Done => {
                mark_done(deps, row.tenant_id, row.chat_id, row.id).await?;
                Ok(MessageResult::Ok)
            }
            FileDelete::Failed(err) => {
                tracing::warn!(attachment_id = %row.id, error = %err, "provider file delete failed");
                if record_failure(deps, &row, &err).await? {
                    Ok(MessageResult::Reject(format!(
                        "attachment cleanup: max attempts ({}) reached: {err}",
                        deps.cfg.cleanup_worker.max_attempts
                    )))
                } else {
                    Ok(MessageResult::Retry)
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: AttachmentCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(ev) => ev,
            Err(e) => return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}")),
        };
        match self.process(&ev).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(attachment_id = %ev.attachment_id, error = %e, "attachment cleanup infrastructure error");
                MessageResult::Retry
            }
        }
    }
}

/// Chat cleanup queue handler.
pub struct ChatCleanupHandler {
    deps: Arc<Deps>,
}

impl ChatCleanupHandler {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    async fn process(&self, ev: &ChatCleanupEvent, attempts: i16) -> Result<MessageResult, DomainError> {
        let deps = &*self.deps;
        let scope = AccessScope::for_tenant(ev.tenant_id);
        match load_chat_any(deps, ev.tenant_id, ev.chat_id).await? {
            Some(c) if c.deleted_at.is_some() => {}
            _ => return Ok(MessageResult::Reject("chat is not soft-deleted".to_owned())),
        }

        let pending = {
            let conn = deps.db.conn()?;
            attachment::Entity::find()
                .filter(
                    Condition::all()
                        .add(attachment::Column::ChatId.eq(ev.chat_id))
                        .add(attachment::Column::CleanupStatus.eq(cleanup_status::PENDING)),
                )
                .secure()
                .scope_with(&scope)
                .all(&conn)
                .await?
        };
        let mut still_pending = 0usize;
        let mut any_failed = false;
        for row in pending {
            let Some(file_id) = row.provider_file_id.clone() else {
                mark_done(deps, row.tenant_id, row.chat_id, row.id).await?;
                continue;
            };
            match delete_provider_file(deps, row.tenant_id, &row.storage_backend, &file_id).await {
                FileDelete::Done => mark_done(deps, row.tenant_id, row.chat_id, row.id).await?,
                FileDelete::Failed(err) => {
                    tracing::warn!(attachment_id = %row.id, error = %err, "chat cleanup: provider file delete failed");
                    if record_failure(deps, &row, &err).await? {
                        any_failed = true;
                    } else {
                        still_pending += 1;
                    }
                }
            }
        }
        if still_pending > 0 {
            return Ok(MessageResult::Retry);
        }
        if !any_failed {
            // Rows that reached terminal `failed` on an earlier delivery.
            let conn = deps.db.conn()?;
            any_failed = attachment::Entity::find()
                .filter(
                    Condition::all()
                        .add(attachment::Column::ChatId.eq(ev.chat_id))
                        .add(attachment::Column::CleanupStatus.eq(cleanup_status::FAILED)),
                )
                .secure()
                .scope_with(&scope)
                .count(&conn)
                .await?
                > 0;
        }

        let vs_row = {
            let conn = deps.db.conn()?;
            chat_vector_store::Entity::find()
                .filter(chat_vector_store::Column::ChatId.eq(ev.chat_id))
                .secure()
                .scope_with(&scope)
                .one(&conn)
                .await?
        };
        let Some(vs_row) = vs_row else {
            return Ok(MessageResult::Ok);
        };
        if let Some(vs_id) = vs_row.vector_store_id.as_deref() {
            if any_failed {
                tracing::warn!(chat_id = %ev.chat_id, "deleting vector store with failed attachment cleanups");
            }
            let res = match deps.providers.provider_for_storage_backend(&vs_row.provider) {
                Some(p) => deps.storage.delete_vector_store(&p, ev.tenant_id, vs_id).await,
                None => Err(StorageError::Config(format!(
                    "unknown storage backend '{}'",
                    vs_row.provider
                ))),
            };
            match res {
                Ok(()) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => {
                    let max = deps.cfg.cleanup_worker.max_attempts.max(1);
                    let delivery = u32::try_from(attempts).unwrap_or(0).saturating_add(1);
                    tracing::warn!(chat_id = %ev.chat_id, error = %e, delivery, "vector store delete failed");
                    if delivery >= max {
                        return Ok(MessageResult::Reject(format!(
                            "vector store delete: max attempts ({max}) reached"
                        )));
                    }
                    return Ok(MessageResult::Retry);
                }
            }
        }
        let conn = deps.db.conn()?;
        chat_vector_store::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::Id.eq(vs_row.id))
                    .add(chat_vector_store::Column::ChatId.eq(ev.chat_id)),
            )
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        Ok(MessageResult::Ok)
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: ChatCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(ev) => ev,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        match self.process(&ev, msg.attempts).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(chat_id = %ev.chat_id, error = %e, "chat cleanup infrastructure error");
                MessageResult::Retry
            }
        }
    }
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod cleanup_tests;
