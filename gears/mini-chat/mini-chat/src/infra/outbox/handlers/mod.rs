//! Leased outbox handlers of the five mini-chat queues (DESIGN §5.6, §3.6 cleanup).

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError,
    MiniChatAuditPluginSpecV1, PublishError, UsageEvent,
};
use sea_orm::Condition;
use sea_orm::sea_query::Expr;
use tokio::sync::Mutex;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxHandle, OutboxMessage,
    OutboxProfile, Partitions, WorkerTuning,
};

use super::{AttachmentCleanupPayload, ChatCleanupPayload, ThreadSummaryPayload};
use crate::domain::app::AppServices;
use crate::domain::policy::{PluginLookup, PluginResolver};
use crate::domain::time::now;
use crate::infra::db::entities::attachment;
use crate::infra::db::repo;

/// Audit retries before the event is dead-lettered (about an hour).
const AUDIT_MAX_ATTEMPTS: i16 = 120;
const AUDIT_PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);
const PROVIDER_DELETE_TIMEOUT: Duration = Duration::from_secs(60);

fn attempt_no(msg: &OutboxMessage) -> u32 {
    u32::try_from(msg.attempts.max(0)).unwrap_or(0) + 1
}

// ───────────────────────────── usage ─────────────────────────────

pub struct UsageHandler {
    svc: Arc<AppServices>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed usage payload: {e}")),
        };
        let plugin = match self.svc.policy.plugin().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "usage publish: model policy plugin unavailable");
                return MessageResult::Retry;
            }
        };
        match plugin.publish_usage(ev).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(e)) => {
                tracing::warn!(error = %e, "usage publish failed (transient)");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(e)) => {
                MessageResult::Reject(format!("usage publish failed: {e}"))
            }
        }
    }
}

// ───────────────────────────── audit ─────────────────────────────

pub struct AuditHandler {
    svc: Arc<AppServices>,
    resolver: PluginResolver,
    last_drop_warning: Mutex<Option<Instant>>,
}

impl AuditHandler {
    fn result(&self, r: MessageResult, msg: &OutboxMessage) -> MessageResult {
        let r = match r {
            MessageResult::Retry if msg.attempts + 1 >= AUDIT_MAX_ATTEMPTS => {
                MessageResult::Reject(format!(
                    "audit delivery gave up after {AUDIT_MAX_ATTEMPTS} attempts"
                ))
            }
            other => other,
        };
        let label = match &r {
            MessageResult::Ok => "ok",
            MessageResult::Retry => "retry",
            MessageResult::Reject(_) => "reject",
        };
        self.svc.metrics.inc("audit_emit", &[("result", label)]);
        r
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: MiniChatAuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                return self.result(
                    MessageResult::Reject(format!("malformed audit payload: {e}")),
                    msg,
                );
            }
        };
        let lookup = self
            .resolver
            .lookup::<dyn MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1>()
            .await;
        let plugin = match lookup {
            Ok(PluginLookup::Found(p)) => p,
            Ok(PluginLookup::NotRegistered) => {
                let mut last = self.last_drop_warning.lock().await;
                if last.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
                    tracing::warn!(
                        "no mini-chat audit plugin registered; audit events are dropped"
                    );
                    *last = Some(Instant::now());
                }
                self.svc.metrics.inc("audit_emit", &[("result", "dropped")]);
                return MessageResult::Ok;
            }
            Ok(PluginLookup::ClientMissing(id)) => {
                tracing::warn!(instance = %id, "audit plugin resolved without a ClientHub client");
                return self.result(MessageResult::Retry, msg);
            }
            Err(e) => {
                tracing::warn!(error = %e, "audit plugin resolution failed");
                return self.result(MessageResult::Retry, msg);
            }
        };
        let r = match tokio::time::timeout(AUDIT_PLUGIN_TIMEOUT, plugin.emit(ev)).await {
            Err(_)
            | Ok(Err(
                MiniChatAuditPluginError::PluginTimeout | MiniChatAuditPluginError::Transient(_),
            )) => MessageResult::Retry,
            Ok(Err(MiniChatAuditPluginError::Permanent(e))) => {
                MessageResult::Reject(format!("audit plugin rejected: {e}"))
            }
            Ok(Ok(())) => MessageResult::Ok,
        };
        self.result(r, msg)
    }
}

// ───────────────────────────── attachment cleanup ─────────────────────────────

/// Outcome of one provider file delete attempt recorded on the attachment.
enum FileCleanup {
    Done,
    Retry,
    Failed,
}

/// Deletes the provider file of an attachment and records the cleanup outcome.
async fn cleanup_attachment_file(
    svc: &AppServices,
    a: &attachment::Model,
) -> Result<FileCleanup, String> {
    let ts = now();
    let mark = |status: &'static str, err: Option<String>| {
        let mut cols = vec![
            (attachment::Column::CleanupStatus, Expr::value(status)),
            (attachment::Column::CleanupUpdatedAt, Expr::value(ts)),
        ];
        if let Some(e) = err {
            cols.push((attachment::Column::LastCleanupError, Expr::value(e)));
        }
        cols
    };
    let conn = svc.db.conn().map_err(|e| e.to_string())?;
    let Some(fid) = a.provider_file_id.clone() else {
        repo::update_attachment_where(
            &conn,
            a.tenant_id,
            a.id,
            Condition::all(),
            mark("done", None),
        )
        .await
        .map_err(|e| e.to_string())?;
        return Ok(FileCleanup::Done);
    };
    let provider_id = svc
        .llm
        .provider_for_backend(&a.storage_backend)
        .unwrap_or_else(|| a.storage_backend.clone());
    let result = match svc.llm.resolve(&provider_id, a.tenant_id) {
        Ok(p) => tokio::time::timeout(PROVIDER_DELETE_TIMEOUT, svc.llm.delete_file(&p, &fid))
            .await
            .unwrap_or_else(|_| {
                Err(crate::infra::llm::ProviderCallError::Timeout(
                    "delete timed out".to_owned(),
                ))
            })
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match result {
        Ok(()) => {
            repo::update_attachment_where(
                &conn,
                a.tenant_id,
                a.id,
                Condition::all(),
                mark("done", None),
            )
            .await
            .map_err(|e| e.to_string())?;
            svc.metrics
                .inc("cleanup_completed", &[("resource_type", "file")]);
            Ok(FileCleanup::Done)
        }
        Err(err) => {
            let attempts = a.cleanup_attempts + 1;
            let max = i32::try_from(svc.cfg.cleanup_worker.max_attempts).unwrap_or(i32::MAX);
            let mut cols = vec![
                (attachment::Column::CleanupAttempts, Expr::value(attempts)),
                (
                    attachment::Column::LastCleanupError,
                    Expr::value(err.chars().take(1000).collect::<String>()),
                ),
                (attachment::Column::CleanupUpdatedAt, Expr::value(ts)),
            ];
            let outcome = if attempts >= max {
                cols.push((attachment::Column::CleanupStatus, Expr::value("failed")));
                svc.metrics
                    .inc("cleanup_failed", &[("resource_type", "file")]);
                FileCleanup::Failed
            } else {
                svc.metrics.inc(
                    "cleanup_retry",
                    &[("resource_type", "file"), ("reason", "provider_error")],
                );
                FileCleanup::Retry
            };
            repo::update_attachment_where(&conn, a.tenant_id, a.id, Condition::all(), cols)
                .await
                .map_err(|e| e.to_string())?;
            Ok(outcome)
        }
    }
}

pub struct AttachmentCleanupHandler {
    svc: Arc<AppServices>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}"));
            }
        };
        let Ok(conn) = self.svc.db.conn() else {
            return MessageResult::Retry;
        };
        let Ok(chat) = repo::find_chat_any(&conn, p.tenant_id, p.chat_id).await else {
            return MessageResult::Retry;
        };
        if chat.as_ref().is_none_or(|c| c.deleted_at.is_some()) {
            // Chat-deletion cleanup owns the provider files of a deleted chat.
            return MessageResult::Ok;
        }
        let att = match repo::find_attachment_by_id(&conn, p.tenant_id, p.attachment_id).await {
            Ok(Some(a)) => a,
            Ok(None) => return MessageResult::Ok,
            Err(_) => return MessageResult::Retry,
        };
        if matches!(att.cleanup_status.as_deref(), Some("done" | "failed")) {
            return MessageResult::Ok;
        }
        let mut att = att;
        if att.provider_file_id.is_none() {
            att.provider_file_id.clone_from(&p.provider_file_id);
        }
        match cleanup_attachment_file(&self.svc, &att).await {
            Ok(FileCleanup::Done) => MessageResult::Ok,
            Ok(FileCleanup::Retry) | Err(_) => MessageResult::Retry,
            Ok(FileCleanup::Failed) => MessageResult::Reject(format!(
                "attachment {} cleanup failed after max attempts",
                att.id
            )),
        }
    }
}

// ───────────────────────────── chat cleanup ─────────────────────────────

pub struct ChatCleanupHandler {
    svc: Arc<AppServices>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        let svc = &self.svc;
        let Ok(conn) = svc.db.conn() else {
            return MessageResult::Retry;
        };
        match repo::find_chat_any(&conn, p.tenant_id, p.chat_id).await {
            Ok(Some(c)) if c.deleted_at.is_some() => {}
            Ok(_) => return MessageResult::Reject("chat is not soft-deleted".to_owned()),
            Err(_) => return MessageResult::Retry,
        }
        let Ok(atts) = repo::chat_attachments_all(&conn, p.tenant_id, p.chat_id).await else {
            return MessageResult::Retry;
        };
        let mut pending = false;
        let mut any_failed = false;
        for a in &atts {
            match a.cleanup_status.as_deref() {
                Some("pending") => match cleanup_attachment_file(svc, a).await {
                    Ok(FileCleanup::Done) => {}
                    Ok(FileCleanup::Failed) => any_failed = true,
                    Ok(FileCleanup::Retry) | Err(_) => pending = true,
                },
                Some("failed") => any_failed = true,
                _ => {}
            }
        }
        if pending {
            return MessageResult::Retry;
        }
        let Ok(conn) = svc.db.conn() else {
            return MessageResult::Retry;
        };
        let Ok(vs) = repo::find_vector_store(&conn, p.tenant_id, p.chat_id).await else {
            return MessageResult::Retry;
        };
        let Some(vs) = vs else {
            return MessageResult::Ok;
        };
        if any_failed {
            svc.metrics
                .inc("cleanup_vector_store_with_failed_attachments", &[]);
        }
        let Some(vs_id) = vs.vector_store_id.clone() else {
            if let Err(e) = repo::delete_vector_store_row(&conn, p.tenant_id, vs.id).await {
                tracing::debug!(error = %e, "best-effort delete of the vector store row failed");
            }
            return MessageResult::Ok;
        };
        let provider_id = svc
            .llm
            .provider_for_backend(&vs.provider)
            .unwrap_or_else(|| vs.provider.clone());
        let res = match svc.llm.resolve(&provider_id, p.tenant_id) {
            Ok(prov) => svc
                .llm
                .delete_vector_store(&prov, &vs_id)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match res {
            Ok(()) => match repo::delete_vector_store_row(&conn, p.tenant_id, vs.id).await {
                Ok(_) => {
                    svc.metrics
                        .inc("cleanup_completed", &[("resource_type", "vector_store")]);
                    MessageResult::Ok
                }
                Err(_) => MessageResult::Retry,
            },
            Err(e) => {
                svc.metrics.inc(
                    "cleanup_retry",
                    &[
                        ("resource_type", "vector_store"),
                        ("reason", "vector_store_delete_failed"),
                    ],
                );
                let max = svc.cfg.cleanup_worker.max_attempts;
                if attempt_no(msg) >= max {
                    svc.metrics
                        .inc("cleanup_failed", &[("resource_type", "vector_store")]);
                    MessageResult::Reject(format!(
                        "vector store delete: max attempts ({max}) reached: {e}"
                    ))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}

// ───────────────────────────── thread summary ─────────────────────────────

pub struct ThreadSummaryHandler {
    svc: Arc<AppServices>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                return MessageResult::Reject(format!("malformed thread summary payload: {e}"));
            }
        };
        let r = crate::domain::thread_summary::run_summary(&self.svc, &p).await;
        match r {
            MessageResult::Retry
                if attempt_no(msg) >= self.svc.cfg.thread_summary_worker.max_attempts =>
            {
                MessageResult::Reject("thread summary: max attempts reached".to_owned())
            }
            other => other,
        }
    }
}

/// Starts the outbox pipeline with the five mini-chat queues.
///
/// # Errors
/// Outbox start failures.
pub async fn start(db: toolkit_db::Db, svc: &Arc<AppServices>) -> anyhow::Result<OutboxHandle> {
    let o = &svc.cfg.outbox;
    let parts = Partitions::of(u16::try_from(o.num_partitions).unwrap_or(4));
    let summary_lease = Duration::from_secs(svc.cfg.thread_summary_worker.claim_timeout_secs);
    let handle = Outbox::builder(db)
        .profile(OutboxProfile::low_latency())
        .processors(2)
        .maintenance(1, 1)
        .processor_tuning(
            WorkerTuning::processor_low_latency()
                .batch_size(1)
                .retry_base(Duration::from_secs(1))
                .retry_max(Duration::from_secs(30)),
        )
        .queue(&o.queue_name, parts)
        .leased(UsageHandler {
            svc: Arc::clone(svc),
        })
        .queue(&o.cleanup_queue_name, parts)
        .leased(AttachmentCleanupHandler {
            svc: Arc::clone(svc),
        })
        .queue(&o.chat_cleanup_queue_name, parts)
        .leased(ChatCleanupHandler {
            svc: Arc::clone(svc),
        })
        .queue(&o.thread_summary_queue_name, parts)
        .leased(ThreadSummaryHandler {
            svc: Arc::clone(svc),
        })
        .lease(LeaseConfig {
            duration: summary_lease,
            headroom: Duration::from_secs(5),
        })
        .queue(&o.audit_queue_name, parts)
        .leased(AuditHandler {
            svc: Arc::clone(svc),
            resolver: PluginResolver::new(Arc::clone(&svc.hub), svc.cfg.vendor.clone()),
            last_drop_warning: Mutex::new(None),
        })
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(5),
        })
        .start()
        .await?;
    Ok(handle)
}
