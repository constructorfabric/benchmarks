//! Attachment cleanup and chat cleanup outbox handlers (DESIGN §3.6
//! "Cleanup on Chat Deletion", §4 "Attachment Deletion").

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, ExprTrait, QueryFilter};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::ServiceSlot;
use crate::domain::error::DomainError;
use crate::domain::service::MiniChat;
use crate::infra::db::entity::{attachments, chat_vector_stores, chats};
use crate::infra::db::now;
use crate::infra::outbox::{AttachmentCleanupEvent, ChatCleanupEvent};

/// Outcome of one provider file cleanup attempt.
enum FileCleanup {
    Done,
    /// Failed attempt recorded; `true` when it reached the attempt budget.
    Failed(bool),
}

impl MiniChat {
    async fn chat_any_state(&self, tenant: Uuid, chat_id: Uuid) -> Result<Option<chats::Model>, DomainError> {
        let conn = self.db.conn()?;
        Ok(chats::Entity::find()
            .filter(
                Condition::all()
                    .add(chats::Column::Id.eq(chat_id))
                    .add(chats::Column::TenantId.eq(tenant)),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant))
            .one(&conn)
            .await?)
    }

    async fn mark_cleanup(&self, tenant: Uuid, id: Uuid, status: &str, error: Option<String>, inc: bool) -> Result<(), DomainError> {
        let conn = self.db.conn()?;
        let ts = now();
        let mut upd = attachments::Entity::update_many()
            .col_expr(attachments::Column::CleanupStatus, Expr::value(Some(status.to_owned())))
            .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)));
        if let Some(e) = error {
            upd = upd.col_expr(attachments::Column::LastCleanupError, Expr::value(Some(e)));
        }
        if inc {
            upd = upd.col_expr(
                attachments::Column::CleanupAttempts,
                Expr::col(attachments::Column::CleanupAttempts).add(1),
            );
        }
        upd.filter(attachments::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant))
            .exec(&conn)
            .await?;
        Ok(())
    }

    /// Delete one attachment's provider file and record the outcome.
    async fn cleanup_file(&self, a: &attachments::Model) -> Result<FileCleanup, DomainError> {
        let Some(file_id) = &a.provider_file_id else {
            self.mark_cleanup(a.tenant_id, a.id, "done", None, false).await?;
            return Ok(FileCleanup::Done);
        };
        let target = self
            .resolver
            .storage_by_label(&a.storage_backend, &a.tenant_id.to_string());
        let res = match target {
            Ok(t) => self.storage.delete_file(&t, file_id).await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match res {
            Ok(()) => {
                if let (Some(sf), "uploaded") = (&a.secondary_file_id, a.secondary_status.as_str())
                    && let Some((pid, _)) = self
                        .resolver
                        .providers()
                        .iter()
                        .find(|(_, e)| e.kind == crate::config::ProviderKind::AnthropicMessages)
                    && let Ok(p) = self.resolver.resolve(pid, &a.tenant_id.to_string())
                    && let Err(e) = self.storage.delete_anthropic_file(&p.alias, sf).await
                {
                    tracing::warn!(error = %e, "secondary file delete failed");
                }
                self.mark_cleanup(a.tenant_id, a.id, "done", None, false).await?;
                Ok(FileCleanup::Done)
            }
            Err(e) => {
                let attempts = a.cleanup_attempts + 1;
                let max = i32::try_from(self.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
                let terminal = attempts >= max;
                self.mark_cleanup(a.tenant_id, a.id, if terminal { "failed" } else { "pending" }, Some(e), true)
                    .await?;
                Ok(FileCleanup::Failed(terminal))
            }
        }
    }
}

/// `mini-chat.attachment_cleanup` handler.
pub struct AttachmentCleanupHandler {
    pub slot: ServiceSlot,
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: AttachmentCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}")),
        };
        let Some(svc) = self.slot.get() else {
            return MessageResult::Retry;
        };
        match svc.chat_any_state(ev.tenant_id, ev.chat_id).await {
            Err(_) => return MessageResult::Retry,
            Ok(Some(c)) if c.deleted_at.is_some() => return MessageResult::Ok,
            Ok(_) => {}
        }
        let Ok(conn) = svc.db.conn() else {
            return MessageResult::Retry;
        };
        let row = match attachments::Entity::find()
            .filter(attachments::Column::Id.eq(ev.attachment_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(ev.tenant_id))
            .one(&conn)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => return MessageResult::Ok,
            Err(_) => return MessageResult::Retry,
        };
        if matches!(row.cleanup_status.as_deref(), Some("done" | "failed")) {
            return MessageResult::Ok;
        }
        match svc.cleanup_file(&row).await {
            Ok(FileCleanup::Done) => MessageResult::Ok,
            Ok(FileCleanup::Failed(true)) => {
                MessageResult::Reject(format!("attachment {} cleanup failed after max attempts", row.id))
            }
            Ok(FileCleanup::Failed(false)) | Err(_) => MessageResult::Retry,
        }
    }
}

/// `mini-chat.chat_cleanup` handler.
pub struct ChatCleanupHandler {
    pub slot: ServiceSlot,
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: ChatCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        let Some(svc) = self.slot.get() else {
            return MessageResult::Retry;
        };
        match svc.chat_any_state(ev.tenant_id, ev.chat_id).await {
            Err(_) => return MessageResult::Retry,
            Ok(Some(c)) if c.deleted_at.is_some() => {}
            Ok(_) => return MessageResult::Reject("chat is not soft-deleted".into()),
        }
        let scope = AccessScope::for_tenant(ev.tenant_id);
        let Ok(conn) = svc.db.conn() else {
            return MessageResult::Retry;
        };
        let Ok(pending) = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(ev.chat_id))
                    .add(attachments::Column::CleanupStatus.eq("pending")),
            )
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await
        else {
            return MessageResult::Retry;
        };
        let mut still_pending = false;
        for a in &pending {
            match svc.cleanup_file(a).await {
                Ok(FileCleanup::Done | FileCleanup::Failed(true)) => {}
                Ok(FileCleanup::Failed(false)) => still_pending = true,
                Err(_) => return MessageResult::Retry,
            }
        }
        if still_pending {
            return MessageResult::Retry;
        }
        let Ok(conn) = svc.db.conn() else {
            return MessageResult::Retry;
        };
        let Ok(vs_row) = chat_vector_stores::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_vector_stores::Column::TenantId.eq(ev.tenant_id))
                    .add(chat_vector_stores::Column::ChatId.eq(ev.chat_id)),
            )
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await
        else {
            return MessageResult::Retry;
        };
        let Some(row) = vs_row else {
            return MessageResult::Ok;
        };
        if let Some(vs) = &row.vector_store_id {
            let res = match svc.resolver.storage_by_label(&row.provider, &ev.tenant_id.to_string()) {
                Ok(t) => svc.storage.delete_vector_store(&t, vs).await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            if let Err(e) = res {
                let max = i16::try_from(svc.cfg.cleanup_worker.max_attempts).unwrap_or(i16::MAX);
                if msg.attempts + 1 >= max {
                    return MessageResult::Reject(format!("vector store delete: max attempts ({max}) reached: {e}"));
                }
                return MessageResult::Retry;
            }
        }
        match chat_vector_stores::Entity::delete_many()
            .filter(chat_vector_stores::Column::Id.eq(row.id))
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await
        {
            Ok(_) => MessageResult::Ok,
            Err(_) => MessageResult::Retry,
        }
    }
}
