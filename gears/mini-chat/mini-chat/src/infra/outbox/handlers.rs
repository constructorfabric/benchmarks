//! Outbox handlers: usage, audit, attachment cleanup, chat cleanup, thread summary.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{AuditPluginError, MiniChatAuditEvent, PublishError, UsageEvent};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::outbox::{LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxError, OutboxHandle, OutboxMessage, OutboxProfile, Partitions};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use uuid::Uuid;

use super::{AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload};
use crate::domain::app::{App, now, tenant_scope};
use crate::domain::summary::SummaryResult;
use crate::infra::db::entity::{attachments, chat_vector_stores, chats};
use crate::infra::gateways::Lookup;

const AUDIT_MAX_ATTEMPTS: u32 = 120;
const AUDIT_TIMEOUT: Duration = Duration::from_secs(30);

fn attempt_of(msg: &OutboxMessage) -> u32 {
    u32::try_from(msg.attempts.max(0)).unwrap_or(0) + 1
}

/// Publishes usage events to the model policy plugin.
pub struct UsageHandler {
    pub app: Arc<App>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed usage payload: {e}")),
        };
        let client = match self.app.policy.client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "usage publish: plugin unavailable");
                return MessageResult::Retry;
            }
        };
        match client.publish_usage(ev).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(e)) => {
                tracing::warn!(error = %e, "usage publish transient error");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(e)) => MessageResult::Reject(e),
        }
    }
}

/// Delivers audit events to the audit plugin.
pub struct AuditHandler {
    pub app: Arc<App>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: MiniChatAuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed audit payload: {e}")),
        };
        let last = attempt_of(msg) >= AUDIT_MAX_ATTEMPTS;
        let retry = || {
            if last {
                MessageResult::Reject("audit delivery: max attempts reached".into())
            } else {
                MessageResult::Retry
            }
        };
        let client = match self.app.audit.lookup().await {
            Ok(Lookup::Found(c)) => c,
            Ok(Lookup::NotRegistered) => {
                tracing::warn!("no audit plugin registered; audit event dropped");
                return MessageResult::Ok;
            }
            Err(e) => {
                tracing::warn!(error = %e, "audit plugin resolution failed");
                return retry();
            }
        };
        match tokio::time::timeout(AUDIT_TIMEOUT, client.emit(ev)).await {
            Ok(Ok(())) => MessageResult::Ok,
            Ok(Err(AuditPluginError::Permanent(e))) => MessageResult::Reject(e),
            Ok(Err(AuditPluginError::Transient(_) | AuditPluginError::PluginTimeout)) | Err(_) => retry(),
        }
    }
}

/// Outcome of one provider file delete recorded on the attachment row.
async fn record_attempt(app: &App, tenant_id: Uuid, id: Uuid, ok: bool, error: Option<String>, attempts_so_far: i32) -> Result<bool, String> {
    let conn = app.db.conn().map_err(|e| e.to_string())?;
    let at = now();
    let max = i32::try_from(app.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
    let mut upd = attachments::Entity::update_many().col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(at)));
    let terminal_failed;
    if ok {
        upd = upd.col_expr(attachments::Column::CleanupStatus, Expr::value(Some("done")));
        terminal_failed = false;
    } else {
        let n = attempts_so_far + 1;
        terminal_failed = n >= max;
        upd = upd
            .col_expr(attachments::Column::CleanupAttempts, Expr::value(n))
            .col_expr(attachments::Column::LastCleanupError, Expr::value(error));
        if terminal_failed {
            upd = upd.col_expr(attachments::Column::CleanupStatus, Expr::value(Some("failed")));
        }
    }
    upd.filter(Condition::all().add(attachments::Column::Id.eq(id)))
        .secure()
        .scope_with(&tenant_scope(tenant_id))
        .exec(&conn)
        .await
        .map_err(|e| e.to_string())?;
    Ok(terminal_failed)
}

/// Deletes the provider file of one attachment row; returns the cleanup state after the attempt.
async fn cleanup_one(app: &App, a: &attachments::Model) -> Result<&'static str, String> {
    let Some(file_id) = a.provider_file_id.clone() else {
        record_attempt(app, a.tenant_id, a.id, true, None, a.cleanup_attempts).await?;
        return Ok("done");
    };
    let Some(rag) = app.resolver.rag_by_backend(&a.storage_backend, a.tenant_id) else {
        let failed = record_attempt(
            app,
            a.tenant_id,
            a.id,
            false,
            Some(format!("unknown storage backend '{}'", a.storage_backend)),
            a.cleanup_attempts,
        )
        .await?;
        return Ok(if failed { "failed" } else { "pending" });
    };
    match app.storage.delete_file(&rag, &file_id).await {
        Ok(()) => {
            record_attempt(app, a.tenant_id, a.id, true, None, a.cleanup_attempts).await?;
            Ok("done")
        }
        Err(e) => {
            let failed = record_attempt(app, a.tenant_id, a.id, false, Some(e.to_string()), a.cleanup_attempts).await?;
            Ok(if failed { "failed" } else { "pending" })
        }
    }
}

/// Deletes provider files of deleted / failed attachments.
pub struct AttachmentCleanupHandler {
    pub app: Arc<App>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}")),
        };
        let scope = tenant_scope(p.tenant_id);
        let Ok(conn) = self.app.db.conn() else { return MessageResult::Retry };
        let chat = match chats::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chats::Column::Id.eq(p.chat_id)))
            .one(&conn)
            .await
        {
            Ok(c) => c,
            Err(_) => return MessageResult::Retry,
        };
        if chat.is_some_and(|c| c.deleted_at.is_some()) {
            return MessageResult::Ok;
        }
        let row = match attachments::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(attachments::Column::Id.eq(p.attachment_id)))
            .one(&conn)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => return MessageResult::Ok,
            Err(_) => return MessageResult::Retry,
        };
        drop(conn);
        if row.cleanup_status.as_deref() != Some("pending") {
            return MessageResult::Ok;
        }
        match cleanup_one(&self.app, &row).await {
            Ok("done") => MessageResult::Ok,
            Ok("failed") => MessageResult::Reject("attachment cleanup: max attempts reached".into()),
            Ok(_) | Err(_) => MessageResult::Retry,
        }
    }
}

/// Deletes provider files and the vector store of a soft-deleted chat.
pub struct ChatCleanupHandler {
    pub app: Arc<App>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        let app = &self.app;
        let scope = tenant_scope(p.tenant_id);
        let Ok(conn) = app.db.conn() else { return MessageResult::Retry };
        match chats::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chats::Column::Id.eq(p.chat_id)))
            .one(&conn)
            .await
        {
            Ok(Some(c)) if c.deleted_at.is_some() => {}
            Ok(_) => return MessageResult::Reject("chat is not soft-deleted".into()),
            Err(_) => return MessageResult::Retry,
        }
        let pending = match attachments::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(p.chat_id))
                    .add(attachments::Column::CleanupStatus.eq("pending")),
            )
            .all(&conn)
            .await
        {
            Ok(r) => r,
            Err(_) => return MessageResult::Retry,
        };
        drop(conn);
        let mut still_pending = false;
        for a in &pending {
            match cleanup_one(app, a).await {
                Ok("pending") | Err(_) => still_pending = true,
                Ok(_) => {}
            }
        }
        if still_pending {
            return MessageResult::Retry;
        }
        let Ok(conn) = app.db.conn() else { return MessageResult::Retry };
        let vs = match chat_vector_stores::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(chat_vector_stores::Column::TenantId.eq(p.tenant_id))
                    .add(chat_vector_stores::Column::ChatId.eq(p.chat_id)),
            )
            .one(&conn)
            .await
        {
            Ok(v) => v,
            Err(_) => return MessageResult::Retry,
        };
        let Some(vs) = vs else { return MessageResult::Ok };
        let failed_any = attachments::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(p.chat_id))
                    .add(attachments::Column::CleanupStatus.eq("failed")),
            )
            .count(&conn)
            .await
            .unwrap_or(0)
            > 0;
        if failed_any {
            tracing::warn!(chat_id = %p.chat_id, "deleting vector store with failed attachment cleanups");
        }
        drop(conn);
        if let Some(store_id) = &vs.vector_store_id {
            let Some(rag) = app.resolver.rag_by_backend(&vs.provider, p.tenant_id) else {
                return MessageResult::Reject(format!("unknown storage backend '{}'", vs.provider));
            };
            if let Err(e) = app.storage.delete_vector_store(&rag, store_id).await {
                tracing::warn!(error = %e, "vector store delete failed");
                return if attempt_of(msg) >= app.cfg.cleanup_worker.max_attempts {
                    MessageResult::Reject(format!(
                        "vector store delete: max attempts ({}) reached",
                        app.cfg.cleanup_worker.max_attempts
                    ))
                } else {
                    MessageResult::Retry
                };
            }
        }
        let Ok(conn) = app.db.conn() else { return MessageResult::Retry };
        match chat_vector_stores::Entity::delete_many()
            .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(vs.id)))
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

/// Runs thread-summary work items.
pub struct ThreadSummaryHandler {
    pub app: Arc<App>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed thread summary payload: {e}")),
        };
        match self.app.run_thread_summary(p, attempt_of(msg)).await {
            SummaryResult::Ok(r) => {
                tracing::debug!(result = r, "thread summary executed");
                MessageResult::Ok
            }
            SummaryResult::Retry(r) => {
                tracing::warn!(reason = %r, "thread summary retry");
                MessageResult::Retry
            }
            SummaryResult::Reject(r) => MessageResult::Reject(r),
        }
    }
}

/// Starts the outbox pipeline with the five mini-chat queues and binds the enqueuer.
///
/// # Errors
/// Outbox start errors.
pub async fn start_outbox(app: &Arc<App>) -> Result<OutboxHandle, OutboxError> {
    let o = &app.cfg.outbox;
    let parts = Partitions::of(u16::try_from(o.num_partitions).unwrap_or(4));
    let summary_lease = Duration::from_secs(app.cfg.thread_summary_worker.claim_timeout_secs);
    let handle = Outbox::builder(app.db.db())
        .profile(OutboxProfile::low_latency())
        .processor_tuning(toolkit_db::outbox::WorkerTuning::processor_low_latency().batch_size(1))
        .queue(&o.queue_name, parts)
        .leased(UsageHandler { app: Arc::clone(app) })
        .queue(&o.cleanup_queue_name, parts)
        .leased(AttachmentCleanupHandler { app: Arc::clone(app) })
        .queue(&o.chat_cleanup_queue_name, parts)
        .leased(ChatCleanupHandler { app: Arc::clone(app) })
        .queue(&o.thread_summary_queue_name, parts)
        .leased(ThreadSummaryHandler { app: Arc::clone(app) })
        .lease(LeaseConfig {
            duration: summary_lease,
            headroom: Duration::from_secs(5).min(summary_lease / 2),
        })
        .queue(&o.audit_queue_name, parts)
        .leased(AuditHandler { app: Arc::clone(app) })
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(2),
        })
        .start()
        .await?;
    app.outbox.bind(Arc::clone(handle.outbox()));
    Ok(handle)
}
