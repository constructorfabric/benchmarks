//! Chat cleanup handler (`mini-chat.chat_cleanup`, DESIGN "Cleanup on Chat Deletion").

use std::sync::Arc;

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt};
use toolkit_security::AccessScope;

use super::ChatCleanupEvent;
use super::attachment_cleanup::{RowOutcome, cleanup_row};
use crate::domain::error::DomainError;
use crate::domain::service::Svc;
use crate::infra::db::entities::{attachments, chat_vector_stores, chats};
use crate::infra::llm::storage::StorageClient;

/// Leased handler of the chat cleanup queue.
pub struct ChatCleanupHandler {
    /// Services.
    pub svc: Arc<Svc>,
}

impl ChatCleanupHandler {
    async fn run(&self, ev: &ChatCleanupEvent, attempts: i16) -> Result<MessageResult, DomainError> {
        let scope = AccessScope::for_tenant(ev.tenant_id);
        let conn = self.svc.db.conn()?;
        let chat = chats::Entity::find()
            .filter(Condition::all().add(chats::Column::Id.eq(ev.chat_id)))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?;
        match chat {
            Some(c) if c.deleted_at.is_some() => {}
            _ => return Ok(MessageResult::Reject("chat is not soft-deleted".into())),
        }
        let rows = attachments::Entity::find()
            .filter(Condition::all().add(attachments::Column::ChatId.eq(ev.chat_id)))
            .secure()
            .scope_with(&scope)
            .all(&conn)
            .await?;
        let mut pending = 0;
        let mut failed = 0;
        for a in &rows {
            match a.cleanup_status.as_deref() {
                Some("pending") => match cleanup_row(&self.svc, a).await? {
                    RowOutcome::Done => {}
                    RowOutcome::Pending(_) => pending += 1,
                    RowOutcome::Failed(_) => failed += 1,
                },
                Some("failed") => failed += 1,
                _ => {}
            }
        }
        if pending > 0 {
            return Ok(MessageResult::Retry);
        }
        let Some(vs) = chat_vector_stores::Entity::find()
            .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(ev.chat_id)))
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
        else {
            return Ok(MessageResult::Ok);
        };
        if failed > 0 {
            self.svc.metrics.inc("cleanup_vector_store_with_failed_attachments_total", &[]);
        }
        if let Some(vs_id) = &vs.vector_store_id {
            let storage = StorageClient::new(&self.svc.llm.gateway);
            let res = match self.svc.llm.resolver.storage_target_by_backend(&vs.provider, ev.tenant_id) {
                Ok(t) => storage.delete_vector_store(&t, vs_id).await.map_err(|e| e.detail),
                Err(e) => Err(e.to_string()),
            };
            if let Err(e) = res {
                self.svc
                    .metrics
                    .inc("cleanup_retry_total", &[("resource_type", "vector_store"), ("reason", "vector_store_delete_failed")]);
                if u32::try_from(attempts + 1).unwrap_or(u32::MAX) >= self.svc.cfg.cleanup_worker.max_attempts {
                    self.svc.metrics.inc("cleanup_failed_total", &[("resource_type", "vector_store")]);
                    return Ok(MessageResult::Reject(format!(
                        "vector store delete: max attempts ({}) reached: {e}",
                        self.svc.cfg.cleanup_worker.max_attempts
                    )));
                }
                return Ok(MessageResult::Retry);
            }
            self.svc.metrics.inc("cleanup_completed_total", &[("resource_type", "vector_store")]);
        }
        chat_vector_stores::Entity::delete_many()
            .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(vs.id)))
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
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        match self.run(&ev, msg.attempts).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "chat cleanup infrastructure failure");
                MessageResult::Retry
            }
        }
    }
}
