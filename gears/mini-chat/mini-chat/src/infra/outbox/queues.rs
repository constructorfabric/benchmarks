//! Outbox queue registration (`cpt-cf-mini-chat-algo-gear-foundation-start-outbox`).
//!
//! The five queues, their partitions and leases are described by [`QueueSpec`]s
//! built from the configuration, so the registration is testable without a
//! running outbox.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-outbox:p1

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use toolkit_db::Db;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxHandle, OutboxMessage,
    Partitions,
};
use uuid::Uuid;

use crate::config::{OutboxConfig, ThreadSummaryWorkerConfig};
use crate::infra::db::migrations::OUTBOX_TABLE_PREFIX;

/// Lease of the audit queue.
pub const AUDIT_QUEUE_LEASE: Duration = Duration::from_secs(60);

/// The five queues of the gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueKind {
    /// `outbox.queue_name`
    Usage,
    /// `outbox.cleanup_queue_name`
    AttachmentCleanup,
    /// `outbox.chat_cleanup_queue_name`
    ChatCleanup,
    /// `outbox.thread_summary_queue_name`
    ThreadSummary,
    /// `outbox.audit_queue_name`
    Audit,
}

impl QueueKind {
    /// Every queue in registration order.
    pub const ALL: [Self; 5] = [
        Self::Usage,
        Self::AttachmentCleanup,
        Self::ChatCleanup,
        Self::ThreadSummary,
        Self::Audit,
    ];
}

/// Identifier a producer partitions by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionKey {
    /// `tenant_id`
    TenantId,
    /// `chat_id`
    ChatId,
}

/// Registration of one queue.
#[derive(Debug, Clone)]
pub struct QueueSpec {
    /// Which queue.
    pub kind: QueueKind,
    /// Configured queue name.
    pub name: String,
    /// Partition count (`outbox.num_partitions`).
    pub partitions: u16,
    /// Lease; only the thread-summary and audit queues override the default.
    pub lease: LeaseConfig,
    /// Whether the handler is a placeholder until its owning feature lands.
    pub placeholder_handler: bool,
}

/// Builds the five queue registrations from the configuration.
#[must_use]
pub fn queue_specs(outbox: &OutboxConfig, summary: &ThreadSummaryWorkerConfig) -> Vec<QueueSpec> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-partitions
    let partitions = u16::try_from(outbox.num_partitions).unwrap_or(u16::MAX);
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-partitions
    let default_lease = LeaseConfig::default();
    let lease = |duration: Duration| LeaseConfig {
        duration,
        headroom: default_lease.headroom,
    };
    let spec = |kind, name: &str, lease, placeholder_handler| QueueSpec {
        kind,
        name: name.to_owned(),
        partitions,
        lease,
        placeholder_handler,
    };
    vec![
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-usage-queue
        spec(QueueKind::Usage, &outbox.queue_name, default_lease, false),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-usage-queue
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-attachment-cleanup
        spec(
            QueueKind::AttachmentCleanup,
            &outbox.cleanup_queue_name,
            default_lease,
            true,
        ),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-attachment-cleanup
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-chat-cleanup
        spec(
            QueueKind::ChatCleanup,
            &outbox.chat_cleanup_queue_name,
            default_lease,
            true,
        ),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-chat-cleanup
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-summary-queue
        spec(
            QueueKind::ThreadSummary,
            &outbox.thread_summary_queue_name,
            lease(Duration::from_secs(summary.claim_timeout_secs)),
            true,
        ),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-summary-queue
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-audit-queue
        spec(
            QueueKind::Audit,
            &outbox.audit_queue_name,
            lease(AUDIT_QUEUE_LEASE),
            false,
        ),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-register-audit-queue
    ]
}

/// The identifier the producer of `kind` partitions by.
#[must_use]
pub const fn partition_key_of(kind: QueueKind) -> PartitionKey {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-partitions
    match kind {
        QueueKind::Usage | QueueKind::AttachmentCleanup | QueueKind::Audit => {
            PartitionKey::TenantId
        }
        QueueKind::ChatCleanup | QueueKind::ThreadSummary => PartitionKey::ChatId,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-partitions
}

/// Stable partition of `key` among `partitions` (`0..partitions`).
#[must_use]
pub fn partition_for(key: Uuid, partitions: u16) -> u32 {
    let bytes = key.as_bytes();
    u32::from(u16::from_be_bytes([bytes[14], bytes[15]])) % u32::from(partitions.max(1))
}

/// Producer side of the outbox: queue names and partition selection. Built at
/// `init` without starting the pipeline.
#[derive(Debug)]
pub struct OutboxEnqueuer {
    config: OutboxConfig,
    outbox: OnceLock<Weak<Outbox>>,
}

impl OutboxEnqueuer {
    /// Creates the enqueuer for the configured queues.
    #[must_use]
    pub fn new(config: &OutboxConfig) -> Self {
        Self {
            config: config.clone(),
            outbox: OnceLock::new(),
        }
    }

    /// Binds the started pipeline; `false` when one is already bound.
    pub fn bind(&self, outbox: &Arc<Outbox>) -> bool {
        self.outbox.set(Arc::downgrade(outbox)).is_ok()
    }

    /// The running outbox, `None` before `start` and after `stop`.
    #[must_use]
    pub fn outbox(&self) -> Option<Arc<Outbox>> {
        self.outbox.get().and_then(Weak::upgrade)
    }

    /// Configured name of `kind`.
    #[must_use]
    pub fn queue_name(&self, kind: QueueKind) -> &str {
        match kind {
            QueueKind::Usage => &self.config.queue_name,
            QueueKind::AttachmentCleanup => &self.config.cleanup_queue_name,
            QueueKind::ChatCleanup => &self.config.chat_cleanup_queue_name,
            QueueKind::ThreadSummary => &self.config.thread_summary_queue_name,
            QueueKind::Audit => &self.config.audit_queue_name,
        }
    }

    /// Partition of a message of `kind`: by `tenant_id` or by `chat_id`
    /// according to [`partition_key_of`].
    #[must_use]
    pub fn partition(&self, kind: QueueKind, tenant_id: Uuid, chat_id: Uuid) -> u32 {
        let key = match partition_key_of(kind) {
            PartitionKey::TenantId => tenant_id,
            PartitionKey::ChatId => chat_id,
        };
        partition_for(
            key,
            u16::try_from(self.config.num_partitions).unwrap_or(u16::MAX),
        )
    }
}

/// Handler of the queues whose owning feature has not landed: logs a warning
/// and returns `Retry`.
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-placeholder-handlers
#[derive(Debug, Clone)]
pub struct PlaceholderHandler {
    queue: String,
}

impl PlaceholderHandler {
    /// Creates the placeholder of `queue`.
    #[must_use]
    pub fn new(queue: impl Into<String>) -> Self {
        Self {
            queue: queue.into(),
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for PlaceholderHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        tracing::warn!(
            queue = %self.queue,
            attempts = msg.attempts,
            "no handler is registered for this queue yet, the message is retried"
        );
        MessageResult::Retry
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-placeholder-handlers

/// Handlers of the queues. The usage and audit handlers are required; each of
/// the other three queues keeps the foundation placeholder until its handler is
/// given.
pub struct OutboxHandlers {
    /// Usage queue handler.
    pub usage: Arc<dyn LeasedMessageHandler>,
    /// Audit queue handler.
    pub audit: Arc<dyn LeasedMessageHandler>,
    /// Attachment cleanup queue handler; `None` keeps the placeholder
    /// (`cpt-cf-mini-chat-dod-attachments-cleanup-handler` replaces it).
    pub attachment_cleanup: Option<Arc<dyn LeasedMessageHandler>>,
    /// Chat cleanup queue handler; `None` keeps the placeholder
    /// (`cpt-cf-mini-chat-algo-background-workers-handle-chat-cleanup`
    /// replaces it).
    pub chat_cleanup: Option<Arc<dyn LeasedMessageHandler>>,
    /// Thread-summary queue handler; `None` keeps the placeholder
    /// (`cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`
    /// replaces it).
    pub thread_summary: Option<Arc<dyn LeasedMessageHandler>>,
}

/// Failure to start the outbox pipeline.
#[derive(Debug, thiserror::Error)]
pub enum OutboxStartError {
    /// The configuration cannot be turned into queue registrations.
    #[error("invalid outbox configuration: {0}")]
    Invalid(String),
    /// The pipeline failed to start.
    #[error("outbox pipeline failed to start: {0}")]
    Start(String),
}

/// Delegates to a shared handler (the builder takes handlers by value).
struct SharedHandler(Arc<dyn LeasedMessageHandler>);

#[async_trait]
impl LeasedMessageHandler for SharedHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.0.handle(msg).await
    }
}

/// Starts the shared outbox pipeline with the five queues.
///
/// # Errors
///
/// [`OutboxStartError`] when the configuration is invalid or the pipeline does
/// not start.
pub async fn start_outbox(
    db: &Db,
    outbox: &OutboxConfig,
    summary: &ThreadSummaryWorkerConfig,
    handlers: OutboxHandlers,
) -> Result<OutboxHandle, OutboxStartError> {
    let partitions = u16::try_from(outbox.num_partitions)
        .ok()
        .filter(|n| (1..=64).contains(n) && n.is_power_of_two())
        .ok_or_else(|| {
            OutboxStartError::Invalid(format!(
                "outbox.num_partitions = {} is not a power of two in 1..=64",
                outbox.num_partitions
            ))
        })?;
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-outbox-defaults
    // Backoff, batch sizes, sequencer and vacuum cadence stay at the shared
    // outbox defaults: no profile or tuning is set on the builder.
    let mut builder = Outbox::builder(db.clone())
        .table_prefix(OUTBOX_TABLE_PREFIX)
        .map_err(|error| OutboxStartError::Invalid(error.to_string()))?;
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-outbox-defaults
    for spec in queue_specs(outbox, summary) {
        let queue = builder.queue(&spec.name, Partitions::of(partitions));
        builder = match spec.kind {
            QueueKind::Usage => queue
                .leased(SharedHandler(Arc::clone(&handlers.usage)))
                .lease(spec.lease)
                .done(),
            QueueKind::Audit => queue
                .leased(SharedHandler(Arc::clone(&handlers.audit)))
                .lease(spec.lease)
                .done(),
            QueueKind::AttachmentCleanup => match &handlers.attachment_cleanup {
                Some(handler) => queue
                    .leased(SharedHandler(Arc::clone(handler)))
                    .lease(spec.lease)
                    .done(),
                None => queue
                    .leased(PlaceholderHandler::new(spec.name.clone()))
                    .lease(spec.lease)
                    .done(),
            },
            QueueKind::ChatCleanup => match &handlers.chat_cleanup {
                Some(handler) => queue
                    .leased(SharedHandler(Arc::clone(handler)))
                    .lease(spec.lease)
                    .done(),
                None => queue
                    .leased(PlaceholderHandler::new(spec.name.clone()))
                    .lease(spec.lease)
                    .done(),
            },
            QueueKind::ThreadSummary => match &handlers.thread_summary {
                Some(handler) => queue
                    .leased(SharedHandler(Arc::clone(handler)))
                    .lease(spec.lease)
                    .done(),
                None => queue
                    .leased(PlaceholderHandler::new(spec.name.clone()))
                    .lease(spec.lease)
                    .done(),
            },
        };
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-return-pipeline
    builder
        .start()
        .await
        .map_err(|error| OutboxStartError::Start(error.to_string()))
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-start-outbox:p1:inst-return-pipeline
}

#[cfg(test)]
#[path = "queues_tests.rs"]
mod tests;
