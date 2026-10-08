//! Outbox message handlers: usage, audit, attachment cleanup, chat cleanup
//! and thread summary (DESIGN §5.6 "Shared Outbox Processing Model").

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{AuditEvent, AuditPluginError, PublishError, UsageEvent};
use sea_orm::EntityTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::attachments::AttachmentCleanupPayload;
use crate::domain::chats::ChatCleanupPayload;
use crate::domain::errors::DomainResult;
use crate::domain::finalize::ThreadSummaryPayload;
use crate::domain::state::AppState;
use crate::domain::summary::SummaryOutcome;
use crate::infra::db::entities::{attachments, chats};
use crate::infra::db::repo;
use crate::infra::policy::AuditTarget;

const AUDIT_MAX_ATTEMPTS: i64 = 120;

fn delivery(msg: &OutboxMessage) -> i64 {
    i64::from(msg.attempts) + 1
}

pub struct UsageHandler(pub Arc<AppState>);
pub struct AuditHandler(pub Arc<AppState>);
pub struct AttachmentCleanupHandler(pub Arc<AppState>);
pub struct ChatCleanupHandler(pub Arc<AppState>);
pub struct ThreadSummaryHandler(pub Arc<AppState>);

#[async_trait::async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("bad usage payload: {e}")),
        };
        let client = match self.0.policy.client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "usage publication: policy plugin unavailable");
                return MessageResult::Retry;
            }
        };
        match client.publish_usage(ev).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(e)) => {
                tracing::warn!(error = %e, "usage publication failed (transient)");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(e)) => MessageResult::Reject(e),
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: AuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("bad audit payload: {e}")),
        };
        let last = delivery(msg) >= AUDIT_MAX_ATTEMPTS;
        let retry = |reason: String| {
            if last {
                MessageResult::Reject(format!("audit delivery gave up: {reason}"))
            } else {
                MessageResult::Retry
            }
        };
        match self.0.audit.resolve().await {
            AuditTarget::NoPlugin => MessageResult::Ok,
            AuditTarget::Retry(r) => retry(r),
            AuditTarget::Client(c) => match tokio::time::timeout(Duration::from_secs(30), c.emit(ev)).await {
                Err(_) => retry("plugin timeout".to_owned()),
                Ok(Ok(())) => MessageResult::Ok,
                Ok(Err(AuditPluginError::Transient(e))) => retry(e),
                Ok(Err(AuditPluginError::PluginTimeout)) => retry("plugin timeout".to_owned()),
                Ok(Err(AuditPluginError::Permanent(e))) => MessageResult::Reject(e),
            },
        }
    }
}

async fn chat_row(state: &AppState, tenant_id: Uuid, chat_id: Uuid) -> DomainResult<Option<chats::Model>> {
    let conn = state.db.conn()?;
    Ok(chats::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(Condition::all().add(chats::Column::Id.eq(chat_id)))
        .one(&conn)
        .await?)
}

async fn mark_cleanup(
    state: &AppState,
    tenant_id: Uuid,
    id: Uuid,
    status: &str,
    attempts: Option<i32>,
    error: Option<String>,
) -> DomainResult<()> {
    let conn = state.db.conn()?;
    let mut q = attachments::Entity::update_many()
        .secure()
        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some(status)))
        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(repo::now())));
    if let Some(a) = attempts {
        q = q.col_expr(attachments::Column::CleanupAttempts, Expr::value(a));
    }
    if let Some(e) = error {
        q = q.col_expr(attachments::Column::LastCleanupError, Expr::value(Some(e)));
    }
    q.filter(Condition::all().add(attachments::Column::Id.eq(id)))
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .exec(&conn)
        .await?;
    Ok(())
}

/// Delete the provider file of an attachment row and record the outcome.
/// Returns `Some(true)` done, `Some(false)` terminal failed, `None` retry.
async fn cleanup_attachment_file(state: &AppState, a: &attachments::Model) -> DomainResult<Option<bool>> {
    let Some(file_id) = a.provider_file_id.clone() else {
        mark_cleanup(state, a.tenant_id, a.id, "done", None, None).await?;
        return Ok(Some(true));
    };
    let result = match state.resolver.storage_by_backend(&a.storage_backend, a.tenant_id) {
        Ok(t) => state.storage.delete_file(&t, &file_id).await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match result {
        Ok(()) => {
            mark_cleanup(state, a.tenant_id, a.id, "done", None, None).await?;
            Ok(Some(true))
        }
        Err(e) => {
            let attempts = a.cleanup_attempts + 1;
            let max = i32::try_from(state.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
            if attempts >= max {
                mark_cleanup(state, a.tenant_id, a.id, "failed", Some(attempts), Some(e)).await?;
                Ok(Some(false))
            } else {
                mark_cleanup(state, a.tenant_id, a.id, "pending", Some(attempts), Some(e)).await?;
                Ok(None)
            }
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("bad attachment cleanup payload: {e}")),
        };
        let state = &self.0;
        match chat_row(state, p.tenant_id, p.chat_id).await {
            Ok(Some(c)) if c.deleted_at.is_some() => return MessageResult::Ok,
            Ok(_) => {}
            Err(_) => return MessageResult::Retry,
        }
        let row = match state.db.conn() {
            Ok(conn) => repo::find_attachment(&conn, &AccessScope::for_tenant(p.tenant_id), p.chat_id, p.attachment_id).await,
            Err(e) => Err(e),
        };
        let Ok(row) = row else { return MessageResult::Retry };
        let Some(mut a) = row else { return MessageResult::Ok };
        if matches!(a.cleanup_status.as_deref(), Some("done" | "failed")) {
            return MessageResult::Ok;
        }
        if a.provider_file_id.is_none() {
            a.provider_file_id.clone_from(&p.provider_file_id);
        }
        match cleanup_attachment_file(state, &a).await {
            Ok(Some(true)) => {
                if let Some(sec) = &p.secondary_ref {
                    tracing::info!(file_id = %sec.file_id, "secondary file cleanup skipped (no secondary storage client)");
                }
                MessageResult::Ok
            }
            Ok(Some(false)) => MessageResult::Reject("attachment cleanup: max attempts reached".to_owned()),
            Ok(None) | Err(_) => MessageResult::Retry,
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("bad chat cleanup payload: {e}")),
        };
        let state = &self.0;
        match chat_row(state, p.tenant_id, p.chat_id).await {
            Ok(Some(c)) if c.deleted_at.is_some() => {}
            Ok(_) => return MessageResult::Reject("chat is not soft-deleted".to_owned()),
            Err(_) => return MessageResult::Retry,
        }
        let scope = AccessScope::for_tenant(p.tenant_id);
        let rows = match state.db.conn() {
            Ok(conn) => attachments::Entity::find()
                .secure()
                .scope_with(&scope)
                .filter(Condition::all().add(attachments::Column::ChatId.eq(p.chat_id)))
                .all(&conn)
                .await
                .map_err(crate::domain::errors::DomainError::from),
            Err(e) => Err(e),
        };
        let Ok(rows) = rows else { return MessageResult::Retry };
        let mut pending = false;
        let mut any_failed = false;
        for a in rows.iter().filter(|a| a.cleanup_status.as_deref() == Some("pending")) {
            match cleanup_attachment_file(state, a).await {
                Ok(Some(true)) => {}
                Ok(Some(false)) => any_failed = true,
                Ok(None) => pending = true,
                Err(_) => return MessageResult::Retry,
            }
        }
        any_failed |= rows.iter().any(|a| a.cleanup_status.as_deref() == Some("failed"));
        if pending {
            return MessageResult::Retry;
        }
        let vs_row = match state.db.conn() {
            Ok(conn) => repo::find_vector_store(&conn, &scope, p.tenant_id, p.chat_id).await,
            Err(e) => Err(e),
        };
        let Ok(vs_row) = vs_row else { return MessageResult::Retry };
        let Some(vs_row) = vs_row else { return MessageResult::Ok };
        if any_failed {
            tracing::warn!(chat_id = %p.chat_id, "deleting vector store with failed attachment cleanups");
        }
        if let Some(vs) = &vs_row.vector_store_id {
            let res = match state.resolver.storage_by_backend(&vs_row.provider, p.tenant_id) {
                Ok(t) => state.storage.delete_vector_store(&t, vs).await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            if let Err(e) = res {
                tracing::warn!(error = %e, "vector store delete failed");
                let max = i64::from(state.cfg.cleanup_worker.max_attempts);
                if delivery(msg) >= max {
                    return MessageResult::Reject(format!("vector store delete: max attempts ({max}) reached"));
                }
                return MessageResult::Retry;
            }
        }
        match state.drop_vector_store_row(p.tenant_id, vs_row.id).await {
            Ok(()) => MessageResult::Ok,
            Err(_) => MessageResult::Retry,
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("bad thread summary payload: {e}")),
        };
        let max = i64::from(self.0.cfg.thread_summary_worker.max_attempts);
        match self.0.run_thread_summary(&p).await {
            Ok(SummaryOutcome::Done(result)) => {
                tracing::info!(chat_id = %p.chat_id, result, "thread summary task finished");
                MessageResult::Ok
            }
            Ok(SummaryOutcome::Reject(r)) => MessageResult::Reject(r),
            Ok(SummaryOutcome::Retry(r)) | Err(crate::domain::errors::DomainError::Internal(r)) => {
                tracing::warn!(chat_id = %p.chat_id, reason = %r, "thread summary task retry");
                if delivery(msg) >= max {
                    MessageResult::Reject(format!("thread summary: {r}"))
                } else {
                    MessageResult::Retry
                }
            }
            Err(e) => {
                if delivery(msg) >= max {
                    MessageResult::Reject(format!("thread summary: {e}"))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}
