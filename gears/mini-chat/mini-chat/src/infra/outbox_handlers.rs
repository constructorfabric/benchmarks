//! Outbox pipeline: five leased queues and their handlers (DESIGN B.9.3).

use std::sync::Arc;
use std::time::Duration;

use toolkit_db::Db;
use toolkit_db::outbox::{
    Batch, HandlerResult, LeaseConfig, LeasedHandler, Outbox, OutboxError, OutboxHandle, OutboxProfile, Partitions, WorkerTuning,
};

use crate::domain::service::MiniChatService;
use crate::domain::service::cleanup::HandlerOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    Usage,
    Audit,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
}

/// Dispatches one queue's messages to the domain handler logic.
pub struct QueueHandler {
    svc: Arc<MiniChatService>,
    kind: QueueKind,
}

impl QueueHandler {
    #[must_use]
    pub fn new(svc: Arc<MiniChatService>, kind: QueueKind) -> Self {
        Self { svc, kind }
    }

    /// Handle one payload (also used directly by tests).
    pub async fn handle_payload(&self, payload: &[u8], attempts: u32) -> HandlerOutcome {
        match self.kind {
            QueueKind::Usage => self.svc.handle_usage(payload).await,
            QueueKind::Audit => self.svc.handle_audit(payload, attempts).await,
            QueueKind::AttachmentCleanup => self.svc.handle_attachment_cleanup(payload).await,
            QueueKind::ChatCleanup => self.svc.handle_chat_cleanup(payload, attempts).await,
            QueueKind::ThreadSummary => self.svc.handle_thread_summary(payload, attempts).await,
        }
    }
}

#[async_trait::async_trait]
impl LeasedHandler for QueueHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult {
        while let Some(msg) = batch.next_msg() {
            let attempts = u32::try_from(msg.attempts.max(0)).unwrap_or(0);
            let payload = msg.payload.clone();
            match self.handle_payload(&payload, attempts).await {
                HandlerOutcome::Ok => batch.ack(),
                HandlerOutcome::Retry(reason) => {
                    tracing::debug!(queue = ?self.kind, %reason, "outbox message retry");
                    return HandlerResult::Retry { reason };
                }
                HandlerOutcome::Reject(reason) => {
                    tracing::warn!(queue = ?self.kind, %reason, "outbox message rejected");
                    batch.reject(reason);
                }
            }
            if batch.should_stop() {
                break;
            }
        }
        HandlerResult::Success
    }
}

fn lease(secs: u64) -> LeaseConfig {
    let headroom = Duration::from_secs(2);
    LeaseConfig { duration: Duration::from_secs(secs.max(5)) + headroom, headroom }
}

/// Start the pipeline and bind the enqueuer.
///
/// # Errors
/// Outbox start failure.
pub async fn start(db: Db, svc: &Arc<MiniChatService>) -> Result<OutboxHandle, OutboxError> {
    let cfg = &svc.cfg.outbox;
    let n = u16::try_from(cfg.num_partitions).unwrap_or(4);
    let parts = || Partitions::of(n);
    let tuning = WorkerTuning::processor_low_latency().batch_size(1).retry_max(Duration::from_secs(30));
    let handle = Outbox::builder(db)
        .profile(OutboxProfile::low_latency())
        .processor_tuning(tuning)
        .queue(&cfg.queue_name, parts())
        .leased(QueueHandler::new(Arc::clone(svc), QueueKind::Usage))
        .queue(&cfg.cleanup_queue_name, parts())
        .leased(QueueHandler::new(Arc::clone(svc), QueueKind::AttachmentCleanup))
        .queue(&cfg.chat_cleanup_queue_name, parts())
        .leased(QueueHandler::new(Arc::clone(svc), QueueKind::ChatCleanup))
        .queue(&cfg.thread_summary_queue_name, parts())
        .leased(QueueHandler::new(Arc::clone(svc), QueueKind::ThreadSummary))
        .lease(lease(svc.cfg.thread_summary_worker.claim_timeout_secs))
        .queue(&cfg.audit_queue_name, parts())
        .leased(QueueHandler::new(Arc::clone(svc), QueueKind::Audit))
        .lease(lease(60))
        .start()
        .await?;
    svc.outbox.bind(Arc::clone(handle.outbox()));
    Ok(handle)
}
