//! Leased outbox handlers and pipeline start (DESIGN §5.6 "Shared Outbox Processing Model").

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{AuditEvent, AuditPluginError, PublishError, UsageEvent};
use toolkit_db::Db;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxError, OutboxHandle,
    OutboxMessage, OutboxProfile, Partitions, WorkerTuning,
};

use crate::domain::service::Core;
use crate::domain::service::attachments::AttachmentCleanupPayload;
use crate::domain::service::chats::ChatCleanupPayload;
use crate::domain::service::finalize::ThreadSummaryPayload;
use crate::domain::service::summary::TaskOutcome;
use crate::infra::plugin_gateway::Resolved;

/// Audit deliveries before an event is dead-lettered.
pub const AUDIT_MAX_ATTEMPTS: u32 = 120;
const AUDIT_TIMEOUT: Duration = Duration::from_secs(30);

fn delivery(msg: &OutboxMessage) -> u32 {
    u32::try_from(msg.attempts.max(0)).unwrap_or(0) + 1
}

fn outcome(o: TaskOutcome) -> MessageResult {
    match o {
        TaskOutcome::Ok => MessageResult::Ok,
        TaskOutcome::Retry(reason) => {
            tracing::debug!(%reason, "mini-chat outbox: retry");
            MessageResult::Retry
        }
        TaskOutcome::Reject(reason) => MessageResult::Reject(reason),
    }
}

pub struct UsageHandler(pub Arc<Core>);

#[async_trait::async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let Ok(ev) = serde_json::from_slice::<UsageEvent>(&msg.payload) else {
            return MessageResult::Reject("malformed usage payload".to_owned());
        };
        let client = match self.0.policy.resolve().await {
            Resolved::Found(c) => c,
            Resolved::NotRegistered | Resolved::Unavailable(_) => return MessageResult::Retry,
        };
        match client.publish_usage(ev).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(_)) => MessageResult::Retry,
            Err(PublishError::Permanent(e)) => MessageResult::Reject(e),
        }
    }
}

pub struct AuditHandler(pub Arc<Core>);

#[async_trait::async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let Ok(ev) = serde_json::from_slice::<AuditEvent>(&msg.payload) else {
            return MessageResult::Reject("malformed audit payload".to_owned());
        };
        let last = delivery(msg) >= AUDIT_MAX_ATTEMPTS;
        let retry = || {
            if last {
                MessageResult::Reject("audit: max attempts reached".to_owned())
            } else {
                MessageResult::Retry
            }
        };
        let client = match self.0.audit.resolve().await {
            Resolved::Found(c) => c,
            Resolved::NotRegistered => {
                tracing::warn!("mini-chat: no audit plugin registered; audit event dropped");
                return MessageResult::Ok;
            }
            Resolved::Unavailable(_) => return retry(),
        };
        let fut = async {
            match ev {
                AuditEvent::Turn(t) => client.emit_turn_audit(t).await,
                AuditEvent::Mutation(m) => client.emit_turn_mutation_audit(m).await,
            }
        };
        match tokio::time::timeout(AUDIT_TIMEOUT, fut).await {
            Ok(Ok(())) => MessageResult::Ok,
            Ok(Err(AuditPluginError::Permanent(e))) => MessageResult::Reject(e),
            Ok(Err(_)) | Err(_) => retry(),
        }
    }
}

pub struct AttachmentCleanupHandler(pub Arc<Core>);

#[async_trait::async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let Ok(p) = serde_json::from_slice::<AttachmentCleanupPayload>(&msg.payload) else {
            return MessageResult::Reject("malformed attachment cleanup payload".to_owned());
        };
        outcome(self.0.process_attachment_cleanup(&p).await)
    }
}

pub struct ChatCleanupHandler(pub Arc<Core>);

#[async_trait::async_trait]
impl LeasedMessageHandler for ChatCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let Ok(p) = serde_json::from_slice::<ChatCleanupPayload>(&msg.payload) else {
            return MessageResult::Reject("malformed chat cleanup payload".to_owned());
        };
        outcome(self.0.process_chat_cleanup(&p, delivery(msg)).await)
    }
}

pub struct ThreadSummaryHandler(pub Arc<Core>);

#[async_trait::async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let Ok(p) = serde_json::from_slice::<ThreadSummaryPayload>(&msg.payload) else {
            return MessageResult::Reject("malformed thread summary payload".to_owned());
        };
        outcome(self.0.process_thread_summary(&p, delivery(msg)).await)
    }
}

/// Starts the outbox pipeline with the five gear queues and binds the dispatch.
///
/// # Errors
/// Outbox start failure.
pub async fn start_outbox(db: Db, core: &Arc<Core>) -> Result<OutboxHandle, OutboxError> {
    let cfg = &core.cfg.outbox;
    let parts = Partitions::of(u16::try_from(cfg.num_partitions).unwrap_or(4));
    let summary_lease = Duration::from_secs(core.cfg.thread_summary_worker.claim_timeout_secs);
    let handle = Outbox::builder(db)
        .profile(OutboxProfile::low_latency())
        .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))
        .queue(&cfg.queue_name, parts)
        .leased(UsageHandler(Arc::clone(core)))
        .queue(&cfg.cleanup_queue_name, parts)
        .leased(AttachmentCleanupHandler(Arc::clone(core)))
        .queue(&cfg.chat_cleanup_queue_name, parts)
        .leased(ChatCleanupHandler(Arc::clone(core)))
        .queue(&cfg.thread_summary_queue_name, parts)
        .leased(ThreadSummaryHandler(Arc::clone(core)))
        .lease(LeaseConfig {
            duration: summary_lease + Duration::from_secs(2),
            headroom: Duration::from_secs(2),
        })
        .queue(&cfg.audit_queue_name, parts)
        .leased(AuditHandler(Arc::clone(core)))
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(2),
        })
        .start()
        .await?;
    core.outbox.bind(handle.outbox());
    Ok(handle)
}
