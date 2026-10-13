//! Outbox handlers: thin adapters from `OutboxMessage` to the domain logic.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use toolkit_db::outbox::{LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxError, OutboxHandle, Partitions, WorkerTuning};

use crate::domain::services::MiniChatService;
use crate::domain::services::background::HandlerOutcome;

/// Which queue a handler serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    Usage,
    Audit,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
}

/// One handler type for all five queues.
pub struct MiniChatHandler {
    svc: Arc<MiniChatService>,
    kind: QueueKind,
}

impl MiniChatHandler {
    #[must_use]
    pub fn new(svc: Arc<MiniChatService>, kind: QueueKind) -> Self {
        Self { svc, kind }
    }
}

fn to_result(kind: QueueKind, o: HandlerOutcome) -> MessageResult {
    match o {
        HandlerOutcome::Ok => MessageResult::Ok,
        HandlerOutcome::Retry(reason) => {
            tracing::debug!(queue = ?kind, %reason, "outbox message will be retried");
            MessageResult::Retry
        }
        HandlerOutcome::Reject(reason) => {
            tracing::warn!(queue = ?kind, %reason, "outbox message rejected (dead-lettered)");
            MessageResult::Reject(reason)
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for MiniChatHandler {
    async fn handle(&self, msg: &toolkit_db::outbox::OutboxMessage) -> MessageResult {
        let attempts = u32::try_from(msg.attempts.max(0)).unwrap_or(0);
        let out = match self.kind {
            QueueKind::Usage => self.svc.handle_usage(&msg.payload).await,
            QueueKind::Audit => self.svc.handle_audit(&msg.payload, attempts).await,
            QueueKind::AttachmentCleanup => self.svc.handle_attachment_cleanup(&msg.payload).await,
            QueueKind::ChatCleanup => self.svc.handle_chat_cleanup(&msg.payload, attempts).await,
            QueueKind::ThreadSummary => self.svc.handle_thread_summary(&msg.payload, attempts).await,
        };
        to_result(self.kind, out)
    }
}

/// Starts the outbox pipeline with the five mini-chat queues.
///
/// # Errors
/// Queue registration failures.
pub async fn start_pipeline(svc: &Arc<MiniChatService>, tuning_low_latency: bool) -> Result<OutboxHandle, OutboxError> {
    let cfg = svc.config().outbox.clone();
    let parts = Partitions::of(u16::try_from(cfg.num_partitions).unwrap_or(4));
    let summary_lease = Duration::from_secs(svc.config().thread_summary_worker.claim_timeout_secs.max(30));
    let tuning = if tuning_low_latency {
        WorkerTuning::processor_low_latency().batch_size(1)
    } else {
        WorkerTuning::processor_default().batch_size(1)
    };
    let h = |k| MiniChatHandler::new(Arc::clone(svc), k);
    Outbox::builder(svc.db().db())
        .processor_tuning(tuning)
        .queue(&cfg.queue_name, parts)
        .leased(h(QueueKind::Usage))
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(5),
        })
        .queue(&cfg.audit_queue_name, parts)
        .leased(h(QueueKind::Audit))
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(5),
        })
        .queue(&cfg.cleanup_queue_name, parts)
        .leased(h(QueueKind::AttachmentCleanup))
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(5),
        })
        .queue(&cfg.chat_cleanup_queue_name, parts)
        .leased(h(QueueKind::ChatCleanup))
        .lease(LeaseConfig {
            duration: Duration::from_secs(300),
            headroom: Duration::from_secs(10),
        })
        .queue(&cfg.thread_summary_queue_name, parts)
        .leased(h(QueueKind::ThreadSummary))
        .lease(LeaseConfig {
            duration: summary_lease,
            headroom: Duration::from_secs(5),
        })
        .start()
        .await
}
