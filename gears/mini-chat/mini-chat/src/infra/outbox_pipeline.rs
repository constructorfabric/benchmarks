//! Consumer side of the gear's shared-outbox queues (DESIGN section 3.2 "Gear
//! lifecycle", section 5.6 "Shared Outbox Processing Model", B.9.3).
//!
//! [`start`] registers the five mini-chat queues ([`MiniChatQueue::ALL`]) on
//! the shared `toolkit-db` outbox (default table prefix, the one the gear
//! migrations create), each with `outbox.num_partitions` partitions, its
//! leased handler and its lease ([`queue_lease`]): thread summary =
//! `thread_summary_worker.claim_timeout_secs`, audit = 60 s, the others the
//! outbox default (30 s). Every other mechanic (retry backoff, reclaim,
//! sequencer, vacuum) uses the shared outbox defaults. The started outbox is
//! attached to the [`OutboxEnqueuer`] so producers can enqueue; the returned
//! [`OutboxPipeline`] stops the workers.
//!
//! Handlers are supplied per queue through [`QueueHandlers`];
//! [`QueueHandlers::for_gear`] installs the usage and audit handlers
//! ([`crate::infra::outbox_handlers`]) and
//! [`QueueHandlers::with_attachment_cleanup`] the attachment cleanup handler;
//! the composition root installs the chat cleanup and thread-summary
//! handlers with [`QueueHandlers::with`]. A queue left without a handler
//! keeps the [`DeferredHandler`]: it keeps every message (`Retry`), so
//! nothing is acknowledged or dead-lettered on its behalf.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::MiniChatModelPolicyPluginClientV1;
use toolkit_db::Db;
use toolkit_db::outbox::{
    Batch, HandlerResult, LeaseConfig, LeasedHandler, LeasedMessageHandler, MessageResult, Outbox,
    OutboxError, OutboxHandle, OutboxMessage,
};
use tracing::debug;

use crate::config::MiniChatConfig;
use crate::infra::audit_gateway::AuditGateway;
use crate::infra::outbox::{MiniChatQueue, OutboxEnqueuer, OutboxQueues};
use crate::infra::outbox_handlers::{AttachmentCleanupHandler, AuditHandler, UsageHandler};

/// Lease of the audit queue (DESIGN section 5.6).
pub const AUDIT_LEASE: Duration = Duration::from_secs(60);

/// Lease of `queue`: thread summary = `thread_summary_worker.claim_timeout_secs`
/// (validated 30..=3600 s), audit = [`AUDIT_LEASE`], the others the outbox
/// default. The ack headroom is the outbox default.
#[must_use]
pub fn queue_lease(queue: MiniChatQueue, config: &MiniChatConfig) -> LeaseConfig {
    let duration = match queue {
        MiniChatQueue::ThreadSummary => {
            Duration::from_secs(config.thread_summary_worker.claim_timeout_secs)
        }
        MiniChatQueue::Audit => AUDIT_LEASE,
        MiniChatQueue::Usage | MiniChatQueue::AttachmentCleanup | MiniChatQueue::ChatCleanup => {
            return LeaseConfig::default();
        }
    };
    let headroom = LeaseConfig::default().headroom;
    LeaseConfig {
        duration: duration.max(headroom.saturating_mul(2)),
        headroom,
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────

/// Handler of a queue whose processing is not available yet: every message
/// is kept for redelivery (`Retry`, with the outbox backoff), never
/// acknowledged nor dead-lettered.
#[derive(Debug, Clone)]
pub struct DeferredHandler {
    queue: String,
}

impl DeferredHandler {
    /// The deferred handler of the queue named `queue`.
    #[must_use]
    pub fn new(queue: impl Into<String>) -> Self {
        Self {
            queue: queue.into(),
        }
    }

    /// Name of the queue the handler serves.
    #[must_use]
    pub fn queue(&self) -> &str {
        &self.queue
    }
}

#[async_trait]
impl LeasedMessageHandler for DeferredHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        debug!(
            target: "mini_chat.outbox",
            queue = %self.queue,
            partition_id = msg.partition_id,
            seq = msg.seq,
            attempts = msg.attempts,
            "outbox message kept for the queue handler"
        );
        MessageResult::Retry
    }
}

/// A leased handler shared behind a trait object.
#[derive(Clone)]
struct SharedHandler(Arc<dyn LeasedHandler>);

#[async_trait]
impl LeasedHandler for SharedHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult {
        self.0.handle(batch).await
    }
}

/// The leased handler of every mini-chat queue.
#[derive(Clone)]
pub struct QueueHandlers {
    usage: Arc<dyn LeasedHandler>,
    attachment_cleanup: Arc<dyn LeasedHandler>,
    chat_cleanup: Arc<dyn LeasedHandler>,
    thread_summary: Arc<dyn LeasedHandler>,
    audit: Arc<dyn LeasedHandler>,
}

impl std::fmt::Debug for QueueHandlers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueHandlers").finish_non_exhaustive()
    }
}

impl QueueHandlers {
    /// A [`DeferredHandler`] for every queue of `queues`.
    #[must_use]
    pub fn deferred(queues: &OutboxQueues) -> Self {
        let deferred = |queue: MiniChatQueue| -> Arc<dyn LeasedHandler> {
            Arc::new(DeferredHandler::new(queues.name(queue)))
        };
        Self {
            usage: deferred(MiniChatQueue::Usage),
            attachment_cleanup: deferred(MiniChatQueue::AttachmentCleanup),
            chat_cleanup: deferred(MiniChatQueue::ChatCleanup),
            thread_summary: deferred(MiniChatQueue::ThreadSummary),
            audit: deferred(MiniChatQueue::Audit),
        }
    }

    /// The gear's handlers: [`UsageHandler`] publishing through `policy`,
    /// [`AuditHandler`] delivering through `audit` (counted under
    /// `metrics_prefix`), the [`DeferredHandler`] for the other queues.
    #[must_use]
    pub fn for_gear(
        queues: &OutboxQueues,
        policy: Arc<dyn MiniChatModelPolicyPluginClientV1>,
        audit: Arc<AuditGateway>,
        metrics_prefix: &str,
    ) -> Self {
        Self::deferred(queues)
            .with(MiniChatQueue::Usage, Arc::new(UsageHandler::new(policy)))
            .with(
                MiniChatQueue::Audit,
                Arc::new(AuditHandler::new(audit, metrics_prefix)),
            )
    }

    /// Installs `handler` on the attachment cleanup queue
    /// (`outbox.cleanup_queue_name`).
    #[must_use]
    pub fn with_attachment_cleanup(self, handler: Arc<AttachmentCleanupHandler>) -> Self {
        self.with(MiniChatQueue::AttachmentCleanup, handler)
    }

    /// Replaces the handler of `queue`.
    #[must_use]
    pub fn with(mut self, queue: MiniChatQueue, handler: Arc<dyn LeasedHandler>) -> Self {
        *self.slot(queue) = handler;
        self
    }

    /// The handler of `queue`.
    #[must_use]
    pub fn get(&self, queue: MiniChatQueue) -> Arc<dyn LeasedHandler> {
        Arc::clone(match queue {
            MiniChatQueue::Usage => &self.usage,
            MiniChatQueue::AttachmentCleanup => &self.attachment_cleanup,
            MiniChatQueue::ChatCleanup => &self.chat_cleanup,
            MiniChatQueue::ThreadSummary => &self.thread_summary,
            MiniChatQueue::Audit => &self.audit,
        })
    }

    fn slot(&mut self, queue: MiniChatQueue) -> &mut Arc<dyn LeasedHandler> {
        match queue {
            MiniChatQueue::Usage => &mut self.usage,
            MiniChatQueue::AttachmentCleanup => &mut self.attachment_cleanup,
            MiniChatQueue::ChatCleanup => &mut self.chat_cleanup,
            MiniChatQueue::ThreadSummary => &mut self.thread_summary,
            MiniChatQueue::Audit => &mut self.audit,
        }
    }
}

// ── Pipeline ─────────────────────────────────────────────────────────────

/// The running outbox pipeline of the gear.
pub struct OutboxPipeline {
    handle: OutboxHandle,
}

impl std::fmt::Debug for OutboxPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxPipeline").finish_non_exhaustive()
    }
}

impl OutboxPipeline {
    /// The running outbox.
    #[must_use]
    pub fn outbox(&self) -> &Arc<Outbox> {
        self.handle.outbox()
    }

    /// Stops the workers and joins them.
    pub async fn stop(self) {
        self.handle.stop().await;
    }
}

/// Registers every mini-chat queue with its handler from `handlers` and its
/// lease from `config`, starts the shared outbox on `db` and attaches it to
/// `enqueuer` (whose queue names and partition count are used).
///
/// # Errors
///
/// [`OutboxError`] when the outbox cannot start (invalid queue registration,
/// database failure).
pub async fn start(
    db: Db,
    config: &MiniChatConfig,
    handlers: &QueueHandlers,
    enqueuer: &OutboxEnqueuer,
) -> Result<OutboxPipeline, OutboxError> {
    let queues = enqueuer.queues();
    let mut builder = Outbox::builder(db);
    for queue in MiniChatQueue::ALL {
        builder = builder
            .queue(queues.name(queue), queues.partitions())
            .leased(SharedHandler(handlers.get(queue)))
            .lease(queue_lease(queue, config))
            .done();
    }
    let handle = builder.start().await?;
    if !enqueuer.attach(Arc::clone(handle.outbox())) {
        debug!(
            target: "mini_chat.outbox",
            "outbox enqueuer already attached; keeping the first outbox"
        );
    }
    Ok(OutboxPipeline { handle })
}

#[cfg(test)]
#[path = "outbox_pipeline_tests.rs"]
mod tests;
