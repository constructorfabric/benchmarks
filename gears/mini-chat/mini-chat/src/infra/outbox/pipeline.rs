//! Outbox pipeline: registers the gear's five leased queues (spec §13.1).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use tokio::sync::watch;
use toolkit_db::Db;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxHandle, OutboxMessage,
    OutboxProfile, Partitions, WorkerTuning,
};
use tracing::debug;

use super::attachment_cleanup::AttachmentCleanupHandler;
use super::audit::AuditHandler;
use super::chat_cleanup::ChatCleanupHandler;
use super::enqueuer::QueueKind;
use super::thread_summary::ThreadSummaryHandler;
use super::usage::UsageHandler;
use crate::config::MiniChatConfig;
use crate::domain::services::cleanup::CleanupService;
use crate::domain::services::thread_summary::ThreadSummaryService;
use crate::infra::db::OUTBOX_TABLE_PREFIX;
use crate::infra::gateways::audit::AuditGateway;
use crate::infra::gateways::model_policy::ModelPolicyGateway;

/// Lease of the audit queue.
const AUDIT_LEASE: Duration = Duration::from_secs(60);
/// Time reserved after handler cancellation for the ack round-trip.
const LEASE_HEADROOM: Duration = Duration::from_secs(2);

/// One handler per queue.
#[derive(Clone)]
pub struct OutboxHandlers {
    pub usage: Arc<dyn LeasedMessageHandler>,
    pub attachment_cleanup: Arc<dyn LeasedMessageHandler>,
    pub chat_cleanup: Arc<dyn LeasedMessageHandler>,
    pub thread_summary: Arc<dyn LeasedMessageHandler>,
    pub audit: Arc<dyn LeasedMessageHandler>,
}

impl OutboxHandlers {
    /// The real usage and audit handlers over `policy` and `audit`;
    /// [`AckAllHandler`] on the queues whose handlers do not exist yet.
    #[must_use]
    pub fn with_gateways(
        policy: Arc<dyn ModelPolicyGateway>,
        audit: Arc<dyn AuditGateway>,
    ) -> Self {
        Self {
            usage: Arc::new(UsageHandler::new(policy)),
            audit: Arc::new(AuditHandler::new(audit)),
            ..Self::placeholders()
        }
    }

    /// The thread-summary queue handled by [`ThreadSummaryHandler`] over `service`.
    #[must_use]
    pub fn with_thread_summary(self, service: Arc<ThreadSummaryService>) -> Self {
        Self {
            thread_summary: Arc::new(ThreadSummaryHandler::new(service)),
            ..self
        }
    }

    /// The attachment-cleanup and chat-cleanup queues handled by
    /// [`AttachmentCleanupHandler`] and [`ChatCleanupHandler`] over `service`.
    #[must_use]
    pub fn with_cleanup(self, service: Arc<CleanupService>) -> Self {
        Self {
            attachment_cleanup: Arc::new(AttachmentCleanupHandler::new(Arc::clone(&service))),
            chat_cleanup: Arc::new(ChatCleanupHandler::new(service)),
            ..self
        }
    }

    /// [`AckAllHandler`] on every queue (until the real handlers exist).
    #[must_use]
    pub fn placeholders() -> Self {
        let ack: Arc<dyn LeasedMessageHandler> = Arc::new(AckAllHandler);
        Self {
            usage: Arc::clone(&ack),
            attachment_cleanup: Arc::clone(&ack),
            chat_cleanup: Arc::clone(&ack),
            thread_summary: Arc::clone(&ack),
            audit: ack,
        }
    }

    /// Handler of `queue`.
    #[must_use]
    pub fn get(&self, queue: QueueKind) -> &Arc<dyn LeasedMessageHandler> {
        match queue {
            QueueKind::Usage => &self.usage,
            QueueKind::AttachmentCleanup => &self.attachment_cleanup,
            QueueKind::ChatCleanup => &self.chat_cleanup,
            QueueKind::ThreadSummary => &self.thread_summary,
            QueueKind::Audit => &self.audit,
        }
    }

    /// Hold the queues whose handlers call providers (attachment cleanup, chat
    /// cleanup, thread summary) until `ready` turns `true`: the outbox starts
    /// before the gear has its S2S context and OAGW upstreams, and a message
    /// handled earlier would only fail and use up a delivery attempt. Usage and
    /// audit run at once.
    #[must_use]
    pub fn gate_provider_queues(self, ready: &watch::Receiver<bool>) -> Self {
        self.map(|queue, inner| match queue {
            QueueKind::AttachmentCleanup | QueueKind::ChatCleanup | QueueKind::ThreadSummary => {
                Arc::new(GatedHandler {
                    ready: ready.clone(),
                    inner,
                })
            }
            QueueKind::Usage | QueueKind::Audit => inner,
        })
    }

    /// Replace every handler by `f(queue, handler)` (e.g. to decorate them).
    #[must_use]
    pub fn map(
        self,
        mut f: impl FnMut(QueueKind, Arc<dyn LeasedMessageHandler>) -> Arc<dyn LeasedMessageHandler>,
    ) -> Self {
        Self {
            usage: f(QueueKind::Usage, self.usage),
            attachment_cleanup: f(QueueKind::AttachmentCleanup, self.attachment_cleanup),
            chat_cleanup: f(QueueKind::ChatCleanup, self.chat_cleanup),
            thread_summary: f(QueueKind::ThreadSummary, self.thread_summary),
            audit: f(QueueKind::Audit, self.audit),
        }
    }
}

/// Placeholder handler: acknowledges every message without action.
pub struct AckAllHandler;

#[async_trait]
impl LeasedMessageHandler for AckAllHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        debug!(
            partition = msg.partition_id,
            seq = msg.seq,
            payload_type = %msg.payload_type,
            "outbox message acknowledged by placeholder handler"
        );
        MessageResult::Ok
    }
}

/// Waits for its gate to open, then delegates; `Retry` without handling when
/// the gate closes unopened (the gear failed to start or is stopping).
struct GatedHandler {
    ready: watch::Receiver<bool>,
    inner: Arc<dyn LeasedMessageHandler>,
}

#[async_trait]
impl LeasedMessageHandler for GatedHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let mut ready = self.ready.clone();
        if ready.wait_for(|open| *open).await.is_err() {
            return MessageResult::Retry;
        }
        self.inner.handle(msg).await
    }
}

/// Adapter registering a shared trait-object handler with the outbox builder.
struct SharedHandler(Arc<dyn LeasedMessageHandler>);

#[async_trait]
impl LeasedMessageHandler for SharedHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.0.handle(msg).await
    }
}

fn lease(duration: Duration) -> LeaseConfig {
    LeaseConfig {
        duration,
        headroom: LEASE_HEADROOM,
    }
}

/// Start the outbox pipeline with the five queues of `cfg.outbox`.
///
/// # Errors
/// Fails on an invalid partition count or when the outbox cannot start.
pub async fn start_outbox(
    db: Db,
    cfg: &MiniChatConfig,
    handlers: OutboxHandlers,
) -> anyhow::Result<OutboxHandle> {
    let o = &cfg.outbox;
    let n = u16::try_from(o.num_partitions).context("outbox.num_partitions")?;
    if !(1..=64).contains(&n) || !n.is_power_of_two() {
        bail!("outbox.num_partitions must be a power of 2 in 1..=64, got {n}");
    }
    let partitions = Partitions::of(n);
    let summary_lease = Duration::from_secs(cfg.thread_summary_worker.claim_timeout_secs);

    let handle = Outbox::builder(db)
        .table_prefix(OUTBOX_TABLE_PREFIX)?
        .profile(OutboxProfile::low_latency())
        // One message per handler call: `attempts` is counted per batch, and the
        // handlers' retry budgets are per message.
        .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))
        .queue(QueueKind::Usage.queue_name(o), partitions)
        .leased(SharedHandler(handlers.usage))
        .queue(QueueKind::AttachmentCleanup.queue_name(o), partitions)
        .leased(SharedHandler(handlers.attachment_cleanup))
        .queue(QueueKind::ChatCleanup.queue_name(o), partitions)
        .leased(SharedHandler(handlers.chat_cleanup))
        .queue(QueueKind::ThreadSummary.queue_name(o), partitions)
        .leased(SharedHandler(handlers.thread_summary))
        .lease(lease(summary_lease))
        .queue(QueueKind::Audit.queue_name(o), partitions)
        .leased(SharedHandler(handlers.audit))
        .lease(lease(AUDIT_LEASE))
        .start()
        .await?;
    Ok(handle)
}
