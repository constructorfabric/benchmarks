//! Outbox cleanup handlers: single-attachment cleanup and chat-deletion
//! cleanup (DESIGN "Cleanup on Chat Deletion", "Attachment Deletion").

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{AccessScope, SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::AppState;
use super::outbox::{AttachmentCleanupEvent, ChatCleanupEvent};
use super::summary::SYSTEM_SUBJECT_ID;
use crate::config::ProviderKind;
use crate::domain::error::DomainResult;
use crate::infra::db::entity::{attachment, chat, chat_vector_store};
use crate::infra::llm::storage::StorageError;
use crate::infra::repo::now_utc;

fn system_ctx(tenant: Uuid) -> Option<SecurityContext> {
    SecurityContext::builder()
        .subject_id(SYSTEM_SUBJECT_ID)
        .subject_tenant_id(tenant)
        .build()
        .ok()
}

/// Outcome of one provider file delete attempt.
enum FileDelete {
    Done,
    Failed(String),
}

impl AppState {
    async fn delete_primary_file(&self, tenant: Uuid, backend: &str, file_id: &str) -> FileDelete {
        let Some(ctx) = system_ctx(tenant) else {
            return FileDelete::Failed("system context unavailable".into());
        };
        let Some(st) = self.llm.registry.storage_by_label(backend, Some(tenant)) else {
            return FileDelete::Failed(format!("unknown storage backend '{backend}'"));
        };
        match self.llm.delete_file(&ctx, &st, file_id).await {
            Ok(_) => FileDelete::Done,
            Err(e) => FileDelete::Failed(e.to_string()),
        }
    }

    async fn delete_secondary(&self, tenant: Uuid, alias: &str, file_id: &str) {
        let Some(ctx) = system_ctx(tenant) else {
            return;
        };
        if let Err(e) = self.llm.delete_anthropic_file(&ctx, alias, file_id).await {
            tracing::warn!(error = %e, "secondary file delete failed");
        }
    }

    fn anthropic_alias(&self, tenant: Uuid) -> Option<String> {
        self.llm
            .registry
            .entries
            .values()
            .find(|e| e.kind == ProviderKind::AnthropicMessages)
            .map(|e| self.llm.registry.alias_for(e, Some(tenant)))
    }

    async fn set_cleanup_done(&self, scope: &AccessScope, id: Uuid) -> DomainResult<()> {
        let conn = self.conn()?;
        let now = now_utc();
        attachment::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(Some("done".to_owned())),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
            .filter(Condition::all().add(attachment::Column::Id.eq(id)))
            .exec(&conn)
            .await?;
        Ok(())
    }

    /// Record a failed attempt; returns `true` when the row became terminal
    /// `failed`.
    async fn record_cleanup_failure(
        &self,
        scope: &AccessScope,
        row: &attachment::Model,
        err: &str,
    ) -> DomainResult<bool> {
        let attempts = row.cleanup_attempts.saturating_add(1);
        let max = i32::try_from(self.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
        let terminal = attempts >= max;
        let conn = self.conn()?;
        let now = now_utc();
        let mut q = attachment::Entity::update_many()
            .secure()
            .scope_with(scope)
            .col_expr(attachment::Column::CleanupAttempts, Expr::value(attempts))
            .col_expr(
                attachment::Column::LastCleanupError,
                Expr::value(Some(err.chars().take(1000).collect::<String>())),
            )
            .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)));
        if terminal {
            q = q.col_expr(
                attachment::Column::CleanupStatus,
                Expr::value(Some("failed".to_owned())),
            );
        }
        q.filter(Condition::all().add(attachment::Column::Id.eq(row.id)))
            .exec(&conn)
            .await?;
        Ok(terminal)
    }

    async fn load_chat_any(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> DomainResult<Option<chat::Model>> {
        let conn = self.conn()?;
        Ok(chat::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
            .one(&conn)
            .await?)
    }

    async fn load_attachment(
        &self,
        scope: &AccessScope,
        id: Uuid,
    ) -> DomainResult<Option<attachment::Model>> {
        let conn = self.conn()?;
        Ok(attachment::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(Condition::all().add(attachment::Column::Id.eq(id)))
            .one(&conn)
            .await?)
    }
}

// ---------------------------------------------------------------------------
// Attachment cleanup
// ---------------------------------------------------------------------------

pub struct AttachmentCleanupHandler {
    pub state: Arc<AppState>,
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: AttachmentCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}"));
            }
        };
        let st = &self.state;
        let scope = AccessScope::for_tenant(ev.tenant_id);
        match st.load_chat_any(&scope, ev.chat_id).await {
            Ok(Some(c)) if c.deleted_at.is_some() => return MessageResult::Ok,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "attachment cleanup: chat lookup failed");
                return MessageResult::Retry;
            }
        }
        let row = match st.load_attachment(&scope, ev.attachment_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return MessageResult::Ok,
            Err(e) => {
                tracing::warn!(error = %e, "attachment cleanup: row lookup failed");
                return MessageResult::Retry;
            }
        };
        if matches!(row.cleanup_status.as_deref(), Some("done" | "failed")) {
            return MessageResult::Ok;
        }
        if let Some(fid) = &ev.provider_file_id {
            match st
                .delete_primary_file(ev.tenant_id, &ev.storage_backend, fid)
                .await
            {
                FileDelete::Done => {}
                FileDelete::Failed(err) => {
                    tracing::warn!(error = %err, attachment_id = %ev.attachment_id, "provider file delete failed");
                    return match st.record_cleanup_failure(&scope, &row, &err).await {
                        Ok(true) => {
                            MessageResult::Reject(format!("attachment cleanup failed: {err}"))
                        }
                        Ok(false) => MessageResult::Retry,
                        Err(e) => {
                            tracing::warn!(error = %e, "attachment cleanup: recording failure failed");
                            MessageResult::Retry
                        }
                    };
                }
            }
            if let Some(sec) = &ev.secondary_ref {
                st.delete_secondary(ev.tenant_id, &sec.upstream_alias, &sec.file_id)
                    .await;
            }
        }
        match st.set_cleanup_done(&scope, ev.attachment_id).await {
            Ok(()) => MessageResult::Ok,
            Err(e) => {
                tracing::warn!(error = %e, "attachment cleanup: marking done failed");
                MessageResult::Retry
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Chat cleanup
// ---------------------------------------------------------------------------

pub struct ChatCleanupHandler {
    pub state: Arc<AppState>,
}

impl ChatCleanupHandler {
    #[allow(
        clippy::cognitive_complexity,
        reason = "sequential orchestration steps; splitting would obscure the flow"
    )]
    async fn run(&self, ev: &ChatCleanupEvent, attempts: i16) -> DomainResult<MessageResult> {
        let st = &self.state;
        let scope = AccessScope::for_tenant(ev.tenant_id);
        match st.load_chat_any(&scope, ev.chat_id).await? {
            Some(c) if c.deleted_at.is_some() => {}
            _ => return Ok(MessageResult::Reject("chat is not soft-deleted".into())),
        }
        let conn = st.conn()?;
        let rows = attachment::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachment::Column::ChatId.eq(ev.chat_id))
                    .add(attachment::Column::CleanupStatus.eq("pending")),
            )
            .all(&conn)
            .await?;
        let mut pending_left = false;
        for row in rows {
            match &row.provider_file_id {
                None => st.set_cleanup_done(&scope, row.id).await?,
                Some(fid) => match st
                    .delete_primary_file(ev.tenant_id, &row.storage_backend, fid)
                    .await
                {
                    FileDelete::Done => {
                        if let (Some(sid), "uploaded") =
                            (&row.secondary_file_id, row.secondary_status.as_str())
                        {
                            if let Some(alias) = st.anthropic_alias(ev.tenant_id) {
                                st.delete_secondary(ev.tenant_id, &alias, sid).await;
                            } else {
                                tracing::warn!(
                                    "secondary file cleanup skipped: no anthropic upstream"
                                );
                            }
                        }
                        st.set_cleanup_done(&scope, row.id).await?;
                    }
                    FileDelete::Failed(err) => {
                        tracing::warn!(error = %err, attachment_id = %row.id, "chat cleanup: file delete failed");
                        if !st.record_cleanup_failure(&scope, &row, &err).await? {
                            pending_left = true;
                        }
                    }
                },
            }
        }
        let max = i32::try_from(st.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
        let last_delivery = i32::from(attempts) + 1 >= max;
        if pending_left {
            return Ok(MessageResult::Retry);
        }
        let conn = st.conn()?;
        let vs = chat_vector_store::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chat_vector_store::Column::ChatId.eq(ev.chat_id)))
            .one(&conn)
            .await?;
        let Some(vs) = vs else {
            return Ok(MessageResult::Ok);
        };
        if let Some(vs_id) = &vs.vector_store_id {
            let failed: Result<(), StorageError> = match (
                system_ctx(ev.tenant_id),
                st.llm
                    .registry
                    .storage_by_label(&vs.provider, Some(ev.tenant_id)),
            ) {
                (Some(ctx), Some(storage)) => st
                    .llm
                    .delete_vector_store(&ctx, &storage, vs_id)
                    .await
                    .map(|_| ()),
                _ => Err(StorageError::Invalid(format!(
                    "unknown storage backend '{}'",
                    vs.provider
                ))),
            };
            if let Err(e) = failed {
                tracing::warn!(error = %e, chat_id = %ev.chat_id, "vector store delete failed");
                if last_delivery {
                    return Ok(MessageResult::Reject(format!(
                        "vector store delete: max attempts ({max}) reached"
                    )));
                }
                return Ok(MessageResult::Retry);
            }
        }
        let conn = st.conn()?;
        chat_vector_store::Entity::delete_many()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chat_vector_store::Column::Id.eq(vs.id)))
            .exec(&conn)
            .await?;
        Ok(MessageResult::Ok)
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: ChatCleanupEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        match self.run(&ev, msg.attempts).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, chat_id = %ev.chat_id, "chat cleanup infrastructure failure");
                MessageResult::Retry
            }
        }
    }
}
