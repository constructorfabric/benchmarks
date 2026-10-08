//! `mini-chat.chat_cleanup` handler: deletes the provider files of a soft-deleted chat, then its
//! vector store once every attachment cleanup is terminal (DESIGN §3.6 "Cleanup on Chat
//! Deletion", B.9.2).

use std::sync::Arc;

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt};
use toolkit_security::AccessScope;

use crate::domain::attachments::{CLEANUP_FAILED, CLEANUP_PENDING, system_ctx, vector_store};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::{attachment, chat_vector_store};
use crate::infra::llm::storage;
use crate::infra::outbox::handlers::attachment_cleanup::{FileCleanup, cleanup_attachment_file, find_chat_any};
use crate::infra::outbox::payloads::ChatCleanupPayload;

pub struct ChatCleanupHandler {
    app: Arc<AppServices>,
}

impl ChatCleanupHandler {
    #[must_use]
    pub fn new(app: Arc<AppServices>) -> Self {
        Self { app }
    }

    async fn process(&self, p: &ChatCleanupPayload, delivery_attempts: i16) -> Result<MessageResult, DomainError> {
        let app = &self.app;
        let Some(chat) = find_chat_any(app, p.tenant_id, p.chat_id).await? else {
            return Ok(MessageResult::Ok);
        };
        if chat.deleted_at.is_none() {
            return Ok(MessageResult::Reject("chat is not soft-deleted".to_owned()));
        }
        let scope = AccessScope::for_tenant(p.tenant_id);
        let rows = {
            let conn = app.db.conn()?;
            attachment::Entity::find()
                .filter(
                    Condition::all()
                        .add(attachment::Column::ChatId.eq(p.chat_id))
                        .add(attachment::Column::CleanupStatus.eq(CLEANUP_PENDING)),
                )
                .secure()
                .scope_with(&scope)
                .all(&conn)
                .await?
        };
        let mut still_pending = false;
        for row in &rows {
            match cleanup_attachment_file(app, row, None).await? {
                FileCleanup::Done | FileCleanup::Failed(_) => {}
                FileCleanup::Pending(_) => still_pending = true,
            }
        }
        if still_pending {
            return Ok(MessageResult::Retry);
        }

        self.cleanup_vector_store(p, delivery_attempts).await
    }

    /// Deletes the chat's vector store (all attachment cleanups are terminal) and its row.
    async fn cleanup_vector_store(&self, p: &ChatCleanupPayload, delivery_attempts: i16) -> Result<MessageResult, DomainError> {
        let app = &self.app;
        let scope = AccessScope::for_tenant(p.tenant_id);
        let Some(vs) = vector_store::find(app, p.tenant_id, p.chat_id).await? else {
            return Ok(MessageResult::Ok);
        };
        if let Some(vs_id) = &vs.vector_store_id {
            self.warn_failed_attachments(p, &scope).await?;
            if let Err(e) = self.delete_store(p, &vs.provider, vs_id).await {
                let max = app.cfg.cleanup_worker.max_attempts;
                let attempt = u32::try_from(delivery_attempts).unwrap_or(0).saturating_add(1);
                tracing::warn!(chat_id = %p.chat_id, attempt, error = %e, "vector store delete failed");
                if attempt >= max {
                    return Ok(MessageResult::Reject(format!("vector store delete: max attempts ({max}) reached")));
                }
                return Ok(MessageResult::Retry);
            }
        }
        let conn = app.db.conn()?;
        chat_vector_store::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(chat_vector_store::Column::Id.eq(vs.id))
                    .add(chat_vector_store::Column::ChatId.eq(p.chat_id)),
            )
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        Ok(MessageResult::Ok)
    }

    async fn warn_failed_attachments(&self, p: &ChatCleanupPayload, scope: &AccessScope) -> Result<(), DomainError> {
        let conn = self.app.db.conn()?;
        let failed = attachment::Entity::find()
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(p.chat_id))
                    .add(attachment::Column::CleanupStatus.eq(CLEANUP_FAILED)),
            )
            .secure()
            .scope_with(scope)
            .count(&conn)
            .await?;
        if failed > 0 {
            tracing::warn!(chat_id = %p.chat_id, failed, "deleting a vector store with failed attachment cleanups");
        }
        Ok(())
    }

    async fn delete_store(&self, p: &ChatCleanupPayload, label: &str, vs_id: &str) -> Result<(), String> {
        let app = &self.app;
        let provider = app.providers.resolve_storage_backend(label, p.tenant_id).map_err(|e| e.to_string())?;
        let ctx = system_ctx(p.tenant_id);
        storage::delete_vector_store(app.transport.as_ref(), &ctx, &provider, vs_id)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        match self.process(&payload, msg.attempts).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(chat_id = %payload.chat_id, error = %e, "chat cleanup: infrastructure error, retrying");
                MessageResult::Retry
            }
        }
    }
}
