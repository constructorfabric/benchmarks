//! Attachment cleanup handler (`mini-chat.attachment_cleanup`, DESIGN §3.6).

use std::sync::Arc;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::AttachmentCleanupEvent;
use crate::domain::error::DomainError;
use crate::domain::service::Svc;
use crate::infra::db::entities::{attachments, chats};
use crate::infra::db::now;
use crate::infra::llm::storage::StorageClient;

/// Cleanup outcome of one attachment row.
pub enum RowOutcome {
    /// Provider file gone; row marked `done`.
    Done,
    /// Delete failed; attempts incremented (still `pending`).
    Pending(String),
    /// Attempts exhausted; row marked `failed`.
    Failed(String),
}

/// Marks a cleanup row.
///
/// # Errors
/// Database connection or update failure.
pub async fn mark_cleanup(
    svc: &Svc,
    tenant_id: Uuid,
    attachment_id: Uuid,
    status: Option<&str>,
    error: Option<String>,
    attempt: bool,
) -> Result<(), DomainError> {
    let conn = svc.db.conn()?;
    let mut q = attachments::Entity::update_many()
        .secure()
        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(now())));
    if let Some(s) = status {
        q = q.col_expr(attachments::Column::CleanupStatus, Expr::value(Some(s.to_owned())));
    }
    if let Some(e) = error {
        q = q.col_expr(attachments::Column::LastCleanupError, Expr::value(Some(e)));
    }
    if attempt {
        use sea_orm::sea_query::ExprTrait as _;
        q = q.col_expr(attachments::Column::CleanupAttempts, Expr::col(attachments::Column::CleanupAttempts).add(1));
    }
    q.filter(Condition::all().add(attachments::Column::Id.eq(attachment_id)))
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(&conn)
        .await?;
    Ok(())
}

/// Deletes the provider file of one attachment row and records the outcome.
///
/// # Errors
/// Infrastructure (DB) errors.
pub async fn cleanup_row(svc: &Svc, a: &attachments::Model) -> Result<RowOutcome, DomainError> {
    let Some(file_id) = a.provider_file_id.clone() else {
        mark_cleanup(svc, a.tenant_id, a.id, Some("done"), None, false).await?;
        return Ok(RowOutcome::Done);
    };
    let storage = StorageClient::new(&svc.llm.gateway);
    let target = svc.llm.resolver.storage_target_by_backend(&a.storage_backend, a.tenant_id);
    let res = match target {
        Ok(t) => storage.delete_file(&t, &file_id).await.map_err(|e| e.detail),
        Err(e) => Err(e.to_string()),
    };
    match res {
        Ok(()) => {
            if let Some(sec) = &a.secondary_file_id {
                let alias = svc
                    .llm
                    .resolver
                    .entries()
                    .iter()
                    .find(|(_, e)| e.kind == crate::config::ProviderKind::AnthropicMessages)
                    .and_then(|(id, _)| svc.llm.resolver.chat_target(id, a.tenant_id).ok())
                    .map(|t| t.alias);
                match alias {
                    Some(alias) => {
                        if let Err(e) = storage.delete_anthropic_file(&alias, sec).await {
                            tracing::warn!(error = %e, "secondary file delete failed");
                        }
                    }
                    None => svc.metrics.inc("secondary_cleanup_skipped_total", &[("provider_kind", "anthropic")]),
                }
            }
            mark_cleanup(svc, a.tenant_id, a.id, Some("done"), None, false).await?;
            svc.metrics.inc("cleanup_completed_total", &[("resource_type", "file")]);
            Ok(RowOutcome::Done)
        }
        Err(e) => {
            let attempts = a.cleanup_attempts + 1;
            if u32::try_from(attempts).unwrap_or(u32::MAX) >= svc.cfg.cleanup_worker.max_attempts {
                mark_cleanup(svc, a.tenant_id, a.id, Some("failed"), Some(e.clone()), true).await?;
                svc.metrics.inc("cleanup_failed_total", &[("resource_type", "file")]);
                Ok(RowOutcome::Failed(e))
            } else {
                mark_cleanup(svc, a.tenant_id, a.id, None, Some(e.clone()), true).await?;
                svc.metrics.inc("cleanup_retry_total", &[("resource_type", "file"), ("reason", "provider_error")]);
                Ok(RowOutcome::Pending(e))
            }
        }
    }
}

/// Leased handler of the attachment cleanup queue.
pub struct AttachmentCleanupHandler {
    /// Services.
    pub svc: Arc<Svc>,
}

impl AttachmentCleanupHandler {
    async fn run(&self, ev: &AttachmentCleanupEvent) -> Result<MessageResult, DomainError> {
        let scope = AccessScope::for_tenant(ev.tenant_id);
        let conn = self.svc.db.conn()?;
        let chat = chats::Entity::find()
            .filter(Condition::all().add(chats::Column::Id.eq(ev.chat_id)))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?;
        if chat.as_ref().is_none_or(|c| c.deleted_at.is_some()) {
            return Ok(MessageResult::Ok);
        }
        let Some(a) = attachments::Entity::find()
            .filter(Condition::all().add(attachments::Column::Id.eq(ev.attachment_id)))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
        else {
            return Ok(MessageResult::Ok);
        };
        if matches!(a.cleanup_status.as_deref(), Some("done" | "failed")) {
            return Ok(MessageResult::Ok);
        }
        let mut a = a;
        if a.provider_file_id.is_none() {
            a.provider_file_id.clone_from(&ev.provider_file_id);
        }
        Ok(match cleanup_row(&self.svc, &a).await? {
            RowOutcome::Done => MessageResult::Ok,
            RowOutcome::Pending(_) => MessageResult::Retry,
            RowOutcome::Failed(e) => MessageResult::Reject(format!("attachment cleanup failed: {e}")),
        })
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: AttachmentCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}")),
        };
        match self.run(&ev).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "attachment cleanup infrastructure failure");
                MessageResult::Retry
            }
        }
    }
}
