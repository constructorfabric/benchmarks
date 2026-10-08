//! Outbox handlers of the five mini-chat queues (DESIGN §5.6, §3.6).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEnvelope, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1, PublishError,
    UsageEvent,
};
use parking_lot::Mutex;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::choose_plugin_instance;
use toolkit_db::Db;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxError, OutboxHandle, OutboxMessage, OutboxProfile,
    Partitions,
};
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::attachments::AttachmentCleanupPayload;
use crate::domain::service::chats::ChatCleanupPayload;
use crate::domain::service::finalize::ThreadSummaryPayload;
use crate::domain::service::summary::SummaryOutcome;
use crate::domain::service::{AppServices, now};
use crate::infra::db::entity::{attachments, chat_vector_stores, chats};
use crate::infra::llm::transport::storage_from_label;

/// Max deliveries of an audit message before it is dead-lettered.
pub const AUDIT_MAX_ATTEMPTS: i16 = 120;
const AUDIT_TIMEOUT: Duration = Duration::from_secs(30);

// ── usage ────────────────────────────────────────────────────────────────

pub struct UsageHandler {
    pub svc: Arc<AppServices>,
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed usage payload: {e}")),
        };
        let Ok(plugin) = self.svc.policy.plugin().await else {
            return MessageResult::Retry;
        };
        match plugin.publish_usage(event).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(_)) => MessageResult::Retry,
            Err(PublishError::Permanent(r)) => MessageResult::Reject(r),
        }
    }
}

// ── audit ────────────────────────────────────────────────────────────────

/// Resolution result of the audit plugin.
pub enum AuditResolution {
    Found(Arc<dyn MiniChatAuditPluginClientV1>),
    NotRegistered,
    Unavailable,
}

/// Source of the audit plugin (no caching of "not registered").
#[async_trait]
pub trait AuditSource: Send + Sync {
    async fn resolve(&self) -> AuditResolution;
}

/// types-registry backed audit plugin lookup.
pub struct RegistryAuditSource {
    pub hub: Arc<ClientHub>,
    pub vendor: String,
    cached: Mutex<Option<String>>,
}

impl RegistryAuditSource {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            cached: Mutex::new(None),
        }
    }
}

#[async_trait]
impl AuditSource for RegistryAuditSource {
    async fn resolve(&self) -> AuditResolution {
        let cached = self.cached.lock().clone();
        let id = match cached {
            Some(id) => id,
            None => {
                let Ok(registry) = self.hub.get::<dyn TypesRegistryClient>() else {
                    return AuditResolution::Unavailable;
                };
                let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
                let Ok(instances) = registry
                    .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
                    .await
                else {
                    return AuditResolution::Unavailable;
                };
                match choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
                    &self.vendor,
                    instances.iter().map(|e| (e.id.as_ref(), &e.object)),
                ) {
                    Ok(id) => {
                        *self.cached.lock() = Some(id.clone());
                        id
                    }
                    Err(toolkit::plugins::ChoosePluginError::PluginNotFound { .. }) => {
                        return AuditResolution::NotRegistered;
                    }
                    Err(_) => return AuditResolution::Unavailable,
                }
            }
        };
        match self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        {
            Some(c) => AuditResolution::Found(c),
            None => {
                *self.cached.lock() = None;
                AuditResolution::Unavailable
            }
        }
    }
}

pub struct AuditHandler {
    pub source: Arc<dyn AuditSource>,
}

impl AuditHandler {
    fn retry(attempts: i16) -> MessageResult {
        if attempts + 1 >= AUDIT_MAX_ATTEMPTS {
            MessageResult::Reject("audit delivery: max attempts reached".to_owned())
        } else {
            MessageResult::Retry
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let envelope: AuditEnvelope = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed audit payload: {e}")),
        };
        let plugin = match self.source.resolve().await {
            AuditResolution::Found(p) => p,
            AuditResolution::NotRegistered => {
                tracing::warn!("no audit plugin registered; audit event dropped");
                return MessageResult::Ok;
            }
            AuditResolution::Unavailable => return Self::retry(msg.attempts),
        };
        let call = async {
            match envelope {
                AuditEnvelope::Turn(e) => plugin.emit_turn_audit(e).await,
                AuditEnvelope::Mutation(e) => plugin.emit_turn_mutation_audit(e).await,
            }
        };
        match tokio::time::timeout(AUDIT_TIMEOUT, call).await {
            Ok(Ok(())) => MessageResult::Ok,
            Ok(Err(MiniChatAuditPluginError::Permanent(r))) => MessageResult::Reject(r),
            Ok(Err(_)) | Err(_) => Self::retry(msg.attempts),
        }
    }
}

// ── cleanup helpers ──────────────────────────────────────────────────────

enum FileCleanup {
    Done,
    Failed,
    Pending,
}

/// Records a provider delete attempt; returns the new cleanup state.
async fn record_cleanup(
    svc: &AppServices,
    tenant_id: Uuid,
    attachment: &attachments::Model,
    result: Result<(), String>,
) -> Result<FileCleanup, DomainError> {
    let conn = svc.conn()?;
    let ts = now();
    let scope = AccessScope::for_tenant(tenant_id);
    match result {
        Ok(()) => {
            attachments::Entity::update_many()
                .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("done")))
                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                .filter(Condition::all().add(attachments::Column::Id.eq(attachment.id)))
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await?;
            Ok(FileCleanup::Done)
        }
        Err(err) => {
            let attempts = attachment.cleanup_attempts + 1;
            let terminal = u32::try_from(attempts).unwrap_or(u32::MAX) >= svc.cfg.cleanup_worker.max_attempts;
            let mut q = attachments::Entity::update_many()
                .col_expr(attachments::Column::CleanupAttempts, Expr::value(attempts))
                .col_expr(attachments::Column::LastCleanupError, Expr::value(Some(err)))
                .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)));
            if terminal {
                q = q.col_expr(attachments::Column::CleanupStatus, Expr::value(Some("failed")));
            }
            q.filter(Condition::all().add(attachments::Column::Id.eq(attachment.id)))
                .secure()
                .scope_with(&scope)
                .exec(&conn)
                .await?;
            Ok(if terminal { FileCleanup::Failed } else { FileCleanup::Pending })
        }
    }
}

async fn delete_provider_file(svc: &AppServices, tenant_id: Uuid, label: &str, file_id: &str) -> Result<(), String> {
    let Some(target) = storage_from_label(&svc.cfg, label, tenant_id) else {
        return Err(format!("unknown storage backend '{label}'"));
    };
    svc.storage
        .delete_file(&target, file_id)
        .await
        .map_err(|e| e.message)
}

// ── attachment cleanup ───────────────────────────────────────────────────

pub struct AttachmentCleanupHandler {
    pub svc: Arc<AppServices>,
}

impl AttachmentCleanupHandler {
    async fn run(&self, p: AttachmentCleanupPayload) -> Result<MessageResult, DomainError> {
        let svc = &self.svc;
        let conn = svc.conn()?;
        let scope = AccessScope::for_tenant(p.tenant_id);
        let chat = chats::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chats::Column::Id.eq(p.chat_id)))
            .one(&conn)
            .await?;
        if chat.is_none_or(|c| c.deleted_at.is_some()) {
            return Ok(MessageResult::Ok);
        }
        let Some(att) = attachments::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(attachments::Column::Id.eq(p.attachment_id)))
            .one(&conn)
            .await?
        else {
            return Ok(MessageResult::Ok);
        };
        drop(conn);
        if matches!(att.cleanup_status.as_deref(), Some("done" | "failed")) {
            return Ok(MessageResult::Ok);
        }
        let result = match &p.provider_file_id {
            None => Ok(()),
            Some(fid) => delete_provider_file(svc, p.tenant_id, &p.storage_backend, fid).await,
        };
        if result.is_ok()
            && let Some(sec) = &p.secondary_ref
        {
            tracing::debug!(file = %sec.file_id, "secondary file delete is best effort");
        }
        Ok(match record_cleanup(svc, p.tenant_id, &att, result).await? {
            FileCleanup::Done => MessageResult::Ok,
            FileCleanup::Pending => MessageResult::Retry,
            FileCleanup::Failed => MessageResult::Reject("attachment cleanup: max attempts reached".to_owned()),
        })
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: AttachmentCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed attachment cleanup payload: {e}")),
        };
        self.run(p).await.unwrap_or(MessageResult::Retry)
    }
}

// ── chat cleanup ─────────────────────────────────────────────────────────

pub struct ChatCleanupHandler {
    pub svc: Arc<AppServices>,
}

impl ChatCleanupHandler {
    async fn run(&self, p: ChatCleanupPayload, attempts: i16) -> Result<MessageResult, DomainError> {
        let svc = &self.svc;
        let scope = AccessScope::for_tenant(p.tenant_id);
        let conn = svc.conn()?;
        let chat = chats::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chats::Column::Id.eq(p.chat_id)))
            .one(&conn)
            .await?;
        match chat {
            Some(c) if c.deleted_at.is_some() => {}
            Some(_) => return Ok(MessageResult::Reject("chat is not soft-deleted".to_owned())),
            None => return Ok(MessageResult::Ok),
        }
        let pending = attachments::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(p.chat_id))
                    .add(attachments::Column::CleanupStatus.eq("pending")),
            )
            .all(&conn)
            .await?;
        drop(conn);
        let mut still_pending = false;
        for att in pending {
            let result = match &att.provider_file_id {
                None => Ok(()),
                Some(fid) => delete_provider_file(svc, p.tenant_id, &att.storage_backend, fid).await,
            };
            if matches!(record_cleanup(svc, p.tenant_id, &att, result).await?, FileCleanup::Pending) {
                still_pending = true;
            }
        }
        if still_pending {
            return Ok(MessageResult::Retry);
        }
        let conn = svc.conn()?;
        let failed_any = attachments::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(p.chat_id))
                    .add(attachments::Column::CleanupStatus.eq("failed")),
            )
            .count(&conn)
            .await?
            > 0;
        let Some(vs) = chat_vector_stores::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chat_vector_stores::Column::ChatId.eq(p.chat_id)))
            .one(&conn)
            .await?
        else {
            return Ok(MessageResult::Ok);
        };
        drop(conn);
        if failed_any {
            tracing::warn!(chat_id = %p.chat_id, "deleting vector store with failed attachment cleanup");
        }
        if let Some(vs_id) = &vs.vector_store_id {
            let Some(target) = storage_from_label(&svc.cfg, &vs.provider, p.tenant_id) else {
                return Ok(MessageResult::Reject(format!("unknown storage backend '{}'", vs.provider)));
            };
            if let Err(e) = svc.storage.delete_vector_store(&target, vs_id).await {
                tracing::warn!(error = %e, chat_id = %p.chat_id, "vector store delete failed");
                if i64::from(attempts) + 1 >= i64::from(svc.cfg.cleanup_worker.max_attempts) {
                    return Ok(MessageResult::Reject(format!(
                        "vector store delete: max attempts ({}) reached",
                        svc.cfg.cleanup_worker.max_attempts
                    )));
                }
                return Ok(MessageResult::Retry);
            }
        }
        chat_vector_stores::Entity::delete_many()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(chat_vector_stores::Column::Id.eq(vs.id)))
            .exec(&svc.conn()?)
            .await?;
        Ok(MessageResult::Ok)
    }
}

#[async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ChatCleanupPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed chat cleanup payload: {e}")),
        };
        self.run(p, msg.attempts).await.unwrap_or(MessageResult::Retry)
    }
}

// ── thread summary ───────────────────────────────────────────────────────

pub struct ThreadSummaryHandler {
    pub svc: Arc<AppServices>,
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let p: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => return MessageResult::Reject(format!("malformed thread summary payload: {e}")),
        };
        match self.svc.run_thread_summary(p, msg.attempts).await {
            SummaryOutcome::Ok(r) => {
                tracing::debug!(result = r, "thread summary handled");
                MessageResult::Ok
            }
            SummaryOutcome::Retry(r) => {
                tracing::debug!(result = r, "thread summary retry");
                MessageResult::Retry
            }
            SummaryOutcome::Reject(r) => MessageResult::Reject(r),
        }
    }
}

/// Starts the outbox pipeline with the five queues and binds the enqueuer.
///
/// # Errors
/// Outbox start failure.
pub async fn start_pipeline(
    db: Db,
    svc: &Arc<AppServices>,
    audit: Arc<dyn AuditSource>,
) -> Result<OutboxHandle, OutboxError> {
    let cfg = &svc.cfg.outbox;
    let parts = Partitions::of(u16::try_from(cfg.num_partitions).unwrap_or(4));
    let summary_lease = Duration::from_secs(svc.cfg.thread_summary_worker.claim_timeout_secs);
    let handle = Outbox::builder(db)
        .profile(OutboxProfile::low_latency())
        .queue(&cfg.queue_name, parts)
        .leased(UsageHandler { svc: Arc::clone(svc) })
        .queue(&cfg.cleanup_queue_name, parts)
        .leased(AttachmentCleanupHandler { svc: Arc::clone(svc) })
        .queue(&cfg.chat_cleanup_queue_name, parts)
        .leased(ChatCleanupHandler { svc: Arc::clone(svc) })
        .queue(&cfg.thread_summary_queue_name, parts)
        .leased(ThreadSummaryHandler { svc: Arc::clone(svc) })
        .lease(LeaseConfig {
            duration: summary_lease,
            headroom: Duration::from_secs(2),
        })
        .queue(&cfg.audit_queue_name, parts)
        .leased(AuditHandler { source: audit })
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(2),
        })
        .start()
        .await?;
    svc.outbox.bind(Arc::clone(handle.outbox()));
    Ok(handle)
}
