//! Transactional outbox integration (DESIGN sections 3.2 and 5.6).
//!
//! The gear owns one seam to the shared outbox: [`OutboxEnqueuer`], created at gear init (the
//! pipeline is not running yet) and bound to the running [`Outbox`] by [`start`]. Domain code never
//! names an outbox type. Queue handlers are registered in [`start`]; a handler per queue is added
//! when the feature that produces the queue's messages exists.

mod attachment_cleanup;
mod audit;
mod chat_cleanup;
mod thread_summary;
mod usage;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use mini_chat_sdk::{TurnAuditEvent, TurnMutationAuditEvent, UsageEvent};
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxError, OutboxHandle,
    OutboxMessage, Partitions, Record, Wake, in_transaction,
};
use toolkit_db::{Db, DbTx};
use uuid::Uuid;

pub use attachment_cleanup::AttachmentCleanupHandler;
pub use audit::AuditOutboxHandler;
pub use chat_cleanup::ChatCleanupHandler;
pub use thread_summary::ThreadSummaryHandler;
pub use usage::UsageOutboxHandler;

use crate::config::{MiniChatConfig, OutboxConfig};
use crate::domain::attachment::CleanupResult;
use crate::domain::cleanup::{AttachmentCleanupPayload, ChatCleanupPayload};
use crate::domain::error::DomainError;
use crate::domain::summary::SummaryWorkItem;
use crate::infra::storage::migrations::OUTBOX_TABLE_PREFIX;

/// Payload type of every JSON message of the gear's queues.
const JSON_PAYLOAD_TYPE: &str = "application/json";

/// Callback observing every message handed to the outbox: `(queue, payload)`.
pub type EnqueueObserver = Arc<dyn Fn(&str, &[u8]) + Send + Sync>;

/// Callback observing the result of every handler invocation: `(queue, result)`.
pub type OutcomeObserver = Arc<dyn Fn(&str, &MessageResult) + Send + Sync>;

/// Enqueues the gear's outbox messages inside the caller's transaction.
pub struct OutboxEnqueuer {
    outbox: OnceLock<Weak<Outbox>>,
    chat_cleanup_queue: String,
    attachment_cleanup_queue: String,
    usage_queue: String,
    audit_queue: String,
    thread_summary_queue: String,
    num_partitions: u32,
    observer: OnceLock<EnqueueObserver>,
    outcome_observer: OnceLock<OutcomeObserver>,
    /// Test-only fault injection: how many of the next usage / audit enqueues fail.
    injected_failures: AtomicU32,
}

/// A message written to the outbox inside a transaction, to be announced to the enqueue observer
/// once the transaction has committed.
#[derive(Debug, Clone)]
pub struct EnqueuedMessage {
    queue: String,
    payload: Vec<u8>,
}

impl OutboxEnqueuer {
    /// Creates an enqueuer that is not bound to a running pipeline yet.
    #[must_use]
    pub fn new(cfg: &OutboxConfig) -> Self {
        Self {
            outbox: OnceLock::new(),
            chat_cleanup_queue: cfg.chat_cleanup_queue_name.clone(),
            attachment_cleanup_queue: cfg.cleanup_queue_name.clone(),
            usage_queue: cfg.queue_name.clone(),
            audit_queue: cfg.audit_queue_name.clone(),
            thread_summary_queue: cfg.thread_summary_queue_name.clone(),
            num_partitions: cfg.num_partitions,
            observer: OnceLock::new(),
            outcome_observer: OnceLock::new(),
            injected_failures: AtomicU32::new(0),
        }
    }

    /// Binds the running pipeline. Only the first binding takes effect.
    ///
    /// # Errors
    /// An error when a pipeline is already bound.
    pub fn bind(&self, outbox: &Arc<Outbox>) -> Result<(), DomainError> {
        self.outbox
            .set(Arc::downgrade(outbox))
            .map_err(|_| DomainError::internal("outbox pipeline already bound"))
    }

    /// Installs an observer of enqueued messages (used by tests); only the first one is kept.
    pub fn observe(&self, observer: EnqueueObserver) {
        if self.observer.set(observer).is_err() {
            tracing::debug!("an enqueue observer is already installed; keeping the first one");
        }
    }

    /// Installs an observer of handler results (used by tests); only the first one is kept.
    pub fn observe_outcomes(&self, observer: OutcomeObserver) {
        if self.outcome_observer.set(observer).is_err() {
            tracing::debug!("an outcome observer is already installed; keeping the first one");
        }
    }

    fn report_outcome(&self, queue: &str, result: &MessageResult) {
        if let Some(observer) = self.outcome_observer.get() {
            observer(queue, result);
        }
    }

    /// Enqueues an arbitrary payload on `queue` in its own transaction, without announcing it to
    /// the enqueue observer (test hook: a message that no gear code path produced).
    ///
    /// # Errors
    /// An internal error when the pipeline is not running or the enqueue fails.
    pub async fn enqueue_raw(
        &self,
        db: &Db,
        queue: &str,
        partition_key: Uuid,
        payload: Vec<u8>,
    ) -> Result<(), DomainError> {
        let outbox = self
            .outbox
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| DomainError::internal("the outbox pipeline is not running"))?;
        let queue = queue.to_owned();
        let partition = partition_of(partition_key, self.num_partitions);
        in_transaction(db, move |tx| {
            Box::pin(async move {
                let record = Record::to(&queue, partition)
                    .payload(payload, JSON_PAYLOAD_TYPE)
                    .build()
                    .map_err(map_outbox_error)?;
                let wake = outbox.enqueue(tx, record).await.map_err(map_outbox_error)?;
                Ok::<_, DomainError>(((), wake))
            })
        })
        .await
    }

    /// Makes the next `count` usage / audit enqueues fail with a transient error, which fails the
    /// finalization transaction they run in (fault injection for tests).
    pub fn inject_enqueue_failures(&self, count: u32) {
        self.injected_failures.store(count, Ordering::SeqCst);
    }

    /// Enqueues the chat-cleanup message of a chat soft delete, partitioned by `chat_id`.
    ///
    /// The returned [`Wake`] must be fired after the transaction commits.
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` when the payload exceeds the outbox size limit, otherwise an
    /// internal error.
    pub async fn enqueue_chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        payload: &ChatCleanupPayload,
    ) -> Result<Wake, DomainError> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|err| DomainError::internal(format!("chat cleanup payload: {err}")))?;
        let partition = partition_of(payload.chat_id, self.num_partitions);
        let record = Record::to(&self.chat_cleanup_queue, partition)
            .payload(bytes.clone(), JSON_PAYLOAD_TYPE)
            .build()
            .map_err(map_outbox_error)?;
        let outbox = self
            .outbox
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| DomainError::internal("the outbox pipeline is not running"))?;
        let wake = outbox.enqueue(tx, record).await.map_err(map_outbox_error)?;
        if let Some(observer) = self.observer.get() {
            observer(&self.chat_cleanup_queue, &bytes);
        }
        Ok(wake)
    }
}

/// The partition of a chat: stable across processes, spread by the UUID tail bytes.
fn partition_of(chat_id: Uuid, num_partitions: u32) -> u32 {
    let bytes = chat_id.as_bytes();
    u32::from(u16::from_be_bytes([bytes[14], bytes[15]])) % num_partitions.max(1)
}

impl OutboxEnqueuer {
    /// Enqueues an attachment cleanup message, partitioned by `tenant_id`, in the caller's
    /// transaction.
    ///
    /// The returned [`Wake`] must be fired after the transaction commits and the
    /// [`EnqueuedMessage`] announced with [`Self::announce`].
    ///
    /// # Errors
    /// An internal error, also when the payload exceeds the outbox size limit (the attachment
    /// `DELETE` reports it as 500, unlike the chat `DELETE`).
    pub async fn enqueue_attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        payload: &AttachmentCleanupPayload,
    ) -> Result<(Wake, EnqueuedMessage), DomainError> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|err| DomainError::internal(format!("attachment cleanup payload: {err}")))?;
        self.enqueue_record(tx, &self.attachment_cleanup_queue, payload.tenant_id, bytes)
            .await
            .map_err(|err| match err {
                DomainError::OutboxPayloadTooLarge { detail } => DomainError::internal(detail),
                other => other,
            })
    }

    /// Enqueues the usage event of a finalization, partitioned by `tenant_id`.
    ///
    /// The returned [`Wake`] must be fired after the transaction commits and the
    /// [`EnqueuedMessage`] announced with [`Self::announce`].
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` or an internal error.
    pub async fn enqueue_usage(
        &self,
        tx: &DbTx<'_>,
        event: &UsageEvent,
    ) -> Result<(Wake, EnqueuedMessage), DomainError> {
        let bytes = serde_json::to_vec(event)
            .map_err(|err| DomainError::internal(format!("usage event payload: {err}")))?;
        self.enqueue_bytes(tx, &self.usage_queue, event.tenant_id, bytes)
            .await
    }

    /// Enqueues the turn audit event of a finalization, partitioned by `tenant_id`.
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` or an internal error.
    pub async fn enqueue_audit(
        &self,
        tx: &DbTx<'_>,
        event: &TurnAuditEvent,
    ) -> Result<(Wake, EnqueuedMessage), DomainError> {
        let bytes = serde_json::to_vec(event)
            .map_err(|err| DomainError::internal(format!("audit event payload: {err}")))?;
        self.enqueue_bytes(tx, &self.audit_queue, event.tenant_id, bytes)
            .await
    }

    /// Enqueues the audit event of a turn mutation (retry, edit or delete), partitioned by
    /// `tenant_id`, in the mutation transaction.
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` or an internal error.
    pub async fn enqueue_mutation_audit(
        &self,
        tx: &DbTx<'_>,
        event: &TurnMutationAuditEvent,
    ) -> Result<(Wake, EnqueuedMessage), DomainError> {
        let bytes = serde_json::to_vec(event)
            .map_err(|err| DomainError::internal(format!("mutation audit payload: {err}")))?;
        self.enqueue_bytes(tx, &self.audit_queue, event.tenant_id(), bytes)
            .await
    }

    /// Enqueues a thread-summary work item, partitioned by `chat_id` so that the tasks of one chat
    /// run one after the other.
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` or an internal error.
    pub async fn enqueue_summary(
        &self,
        tx: &DbTx<'_>,
        item: &SummaryWorkItem,
    ) -> Result<(Wake, EnqueuedMessage), DomainError> {
        let bytes = serde_json::to_vec(item)
            .map_err(|err| DomainError::internal(format!("thread summary payload: {err}")))?;
        self.enqueue_record(tx, &self.thread_summary_queue, item.chat_id, bytes)
            .await
    }

    async fn enqueue_bytes(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        partition_key: Uuid,
        bytes: Vec<u8>,
    ) -> Result<(Wake, EnqueuedMessage), DomainError> {
        if self
            .injected_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(DomainError::internal("injected transient enqueue failure"));
        }
        self.enqueue_record(tx, queue, partition_key, bytes).await
    }

    async fn enqueue_record(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        partition_key: Uuid,
        bytes: Vec<u8>,
    ) -> Result<(Wake, EnqueuedMessage), DomainError> {
        let record = Record::to(queue, partition_of(partition_key, self.num_partitions))
            .payload(bytes.clone(), JSON_PAYLOAD_TYPE)
            .build()
            .map_err(map_outbox_error)?;
        let outbox = self
            .outbox
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| DomainError::internal("the outbox pipeline is not running"))?;
        let wake = outbox.enqueue(tx, record).await.map_err(map_outbox_error)?;
        Ok((
            wake,
            EnqueuedMessage {
                queue: queue.to_owned(),
                payload: bytes,
            },
        ))
    }

    /// Tells the observer about messages whose transaction committed.
    pub fn announce(&self, messages: &[EnqueuedMessage]) {
        if let Some(observer) = self.observer.get() {
            for message in messages {
                observer(&message.queue, &message.payload);
            }
        }
    }
}

fn map_outbox_error(err: OutboxError) -> DomainError {
    match err {
        OutboxError::PayloadTooLarge { size, max } => DomainError::OutboxPayloadTooLarge {
            detail: format!(
                "the cleanup payload of {size} bytes exceeds the outbox limit of {max} bytes"
            ),
        },
        other => DomainError::internal(format!("outbox enqueue: {other}")),
    }
}

/// Lease of the audit queue (DESIGN section 5.6).
const AUDIT_LEASE: LeaseConfig = LeaseConfig {
    duration: Duration::from_secs(60),
    headroom: Duration::from_secs(2),
};

/// Ack headroom of the thread-summary queue lease (the lease itself is
/// `thread_summary_worker.claim_timeout_secs`).
const SUMMARY_LEASE_HEADROOM: Duration = Duration::from_secs(2);

/// The handlers of the queues the pipeline runs.
pub struct QueueHandlers {
    /// Chat-deletion cleanup.
    pub chat_cleanup: ChatCleanupHandler,
    /// Single-attachment cleanup.
    pub attachment_cleanup: AttachmentCleanupHandler,
    /// Usage publication.
    pub usage: UsageOutboxHandler,
    /// Audit delivery.
    pub audit: AuditOutboxHandler,
    /// Thread-summary execution.
    pub thread_summary: ThreadSummaryHandler,
}

/// Reports the result of every invocation of the wrapped handler to the enqueuer's outcome
/// observer (a no-op when none is installed).
struct ObservedHandler<H> {
    queue: String,
    enqueuer: Arc<OutboxEnqueuer>,
    inner: H,
}

#[async_trait::async_trait]
impl<H: LeasedMessageHandler> LeasedMessageHandler for ObservedHandler<H> {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let result = self.inner.handle(msg).await;
        self.enqueuer.report_outcome(&self.queue, &result);
        result
    }
}

fn observe<H>(queue: &str, enqueuer: &Arc<OutboxEnqueuer>, inner: H) -> ObservedHandler<H> {
    ObservedHandler {
        queue: queue.to_owned(),
        enqueuer: Arc::clone(enqueuer),
        inner,
    }
}

/// Maps the outcome of a cleanup handler to the outbox result.
pub(crate) fn cleanup_message_result(result: CleanupResult) -> MessageResult {
    match result {
        CleanupResult::Done => MessageResult::Ok,
        CleanupResult::Retry => MessageResult::Retry,
        CleanupResult::Reject(reason) => MessageResult::Reject(reason),
    }
}

/// Starts the outbox pipeline with the gear's queues and binds the enqueuer to it.
///
/// The pipeline runs the chat-cleanup, attachment-cleanup, usage, audit and thread-summary queues.
/// The thread-summary queue leases a partition for `thread_summary_worker.claim_timeout_secs`
/// so that the non-streaming summary call is not cancelled and redelivered mid-flight.
///
/// # Errors
/// An error when the pipeline cannot start or is already bound.
pub async fn start(
    db: Db,
    cfg: &MiniChatConfig,
    enqueuer: &Arc<OutboxEnqueuer>,
    handlers: QueueHandlers,
) -> anyhow::Result<OutboxHandle> {
    let partitions = u16::try_from(cfg.outbox.num_partitions)
        .map_err(|_| anyhow::anyhow!("outbox.num_partitions does not fit the outbox"))?;
    let handle = Outbox::builder(db)
        .table_prefix(OUTBOX_TABLE_PREFIX)?
        .queue(
            &cfg.outbox.chat_cleanup_queue_name,
            Partitions::of(partitions),
        )
        .leased(observe(
            &cfg.outbox.chat_cleanup_queue_name,
            enqueuer,
            handlers.chat_cleanup,
        ))
        .queue(&cfg.outbox.cleanup_queue_name, Partitions::of(partitions))
        .leased(observe(
            &cfg.outbox.cleanup_queue_name,
            enqueuer,
            handlers.attachment_cleanup,
        ))
        .queue(&cfg.outbox.queue_name, Partitions::of(partitions))
        .leased(observe(&cfg.outbox.queue_name, enqueuer, handlers.usage))
        .queue(&cfg.outbox.audit_queue_name, Partitions::of(partitions))
        .leased(observe(
            &cfg.outbox.audit_queue_name,
            enqueuer,
            handlers.audit,
        ))
        .lease(AUDIT_LEASE)
        .queue(
            &cfg.outbox.thread_summary_queue_name,
            Partitions::of(partitions),
        )
        .leased(observe(
            &cfg.outbox.thread_summary_queue_name,
            enqueuer,
            handlers.thread_summary,
        ))
        .lease(LeaseConfig {
            duration: Duration::from_secs(cfg.thread_summary_worker.claim_timeout_secs),
            headroom: SUMMARY_LEASE_HEADROOM,
        })
        .start()
        .await?;
    enqueuer
        .bind(handle.outbox())
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(handle)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
