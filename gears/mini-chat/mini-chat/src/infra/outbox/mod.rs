//! Transactional outbox: enqueuer, payloads and pipeline.
//!
//! Five leased queues (usage, attachment cleanup, chat cleanup, thread summary, audit) over the
//! platform outbox (`toolkit_db::outbox`, default table prefix). Producers call
//! [`OutboxEnqueuer::enqueue`] inside their database transaction (via
//! [`crate::infra::db::tx::write_tx_with_wakes`], which fires the sequencer wake-up after
//! commit); [`start_outbox`] registers the queues and their handlers.
//! Delivery is at-least-once and ordered per partition, so handlers must be idempotent.

pub mod attachment_cleanup;
pub mod audit;
pub mod chat_cleanup;
pub mod payloads;
pub mod thread_summary;
pub mod usage;

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use mini_chat_sdk::{AuditEvent, UsageEvent};
use serde::Serialize;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxError, OutboxHandle,
    OutboxMessage, OutboxProfile, Partitions, Record, Wake, WorkerTuning,
};
use toolkit_db::{Db, DbTx};
use uuid::Uuid;

pub use payloads::{
    ATTACHMENT_CLEANUP_PAYLOAD_TYPE, AUDIT_PAYLOAD_TYPE, AttachmentCleanupEvent,
    CHAT_CLEANUP_PAYLOAD_TYPE, ChatCleanupEvent, SecondaryRef, THREAD_SUMMARY_PAYLOAD_TYPE,
    ThreadSummaryTask, USAGE_PAYLOAD_TYPE,
};

use crate::config::{MiniChatConfig, OutboxConfig};
use crate::domain::error::DomainError;

/// Maximum payload size accepted by the platform outbox (`64 KiB`).
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
/// Lease of the audit queue (the plugin call itself times out after 30 s).
const AUDIT_LEASE: Duration = Duration::from_secs(60);
/// Time reserved at the end of a lease for the platform to record the outcome.
const LEASE_HEADROOM: Duration = Duration::from_secs(2);
/// Upper bound of the per-message retry backoff.
const RETRY_MAX: Duration = Duration::from_secs(30);

/// One of the five outbox queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueKind {
    Usage,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
    Audit,
}

impl QueueKind {
    /// The configured queue name.
    #[must_use]
    pub fn queue_name(self, cfg: &OutboxConfig) -> &str {
        match self {
            Self::Usage => &cfg.queue_name,
            Self::AttachmentCleanup => &cfg.cleanup_queue_name,
            Self::ChatCleanup => &cfg.chat_cleanup_queue_name,
            Self::ThreadSummary => &cfg.thread_summary_queue_name,
            Self::Audit => &cfg.audit_queue_name,
        }
    }
}

/// Maps a partition key to a partition: the last two bytes of the UUID, big-endian, modulo `n`.
/// Stable, so every message of one key lands in one partition (ordered delivery).
#[must_use]
pub fn partition_for(key: Uuid, n: u32) -> u32 {
    let b = key.as_bytes();
    u32::from(u16::from_be_bytes([b[14], b[15]])) % n
}

/// A serialized message for one queue, ready to enqueue.
#[derive(Debug, Clone)]
pub struct OutboxRecord {
    pub kind: QueueKind,
    /// Partition key: `tenant_id` (usage, audit, attachment cleanup) or `chat_id` (chat cleanup,
    /// thread summary).
    pub key: Uuid,
    pub payload_type: &'static str,
    pub payload: Vec<u8>,
}

impl OutboxRecord {
    /// Wraps already serialized bytes.
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` (`ChatCleanupPayloadTooLarge` for the chat cleanup queue) when the
    /// payload exceeds 64 KiB.
    pub fn new(
        kind: QueueKind,
        key: Uuid,
        payload_type: &'static str,
        payload: Vec<u8>,
    ) -> Result<Self, DomainError> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            let msg = format!(
                "{payload_type} payload is {} bytes, the limit is {MAX_PAYLOAD_BYTES}",
                payload.len()
            );
            return Err(match kind {
                QueueKind::ChatCleanup => DomainError::ChatCleanupPayloadTooLarge(msg),
                _ => DomainError::OutboxPayloadTooLarge(msg),
            });
        }
        Ok(Self {
            kind,
            key,
            payload_type,
            payload,
        })
    }

    fn json<T: Serialize>(
        kind: QueueKind,
        key: Uuid,
        payload_type: &'static str,
        value: &T,
    ) -> Result<Self, DomainError> {
        let payload = serde_json::to_vec(value)
            .map_err(|e| DomainError::Internal(format!("serialize {payload_type}: {e}")))?;
        Self::new(kind, key, payload_type, payload)
    }

    /// Usage event, partitioned by tenant.
    ///
    /// # Errors
    /// Payload larger than 64 KiB, or serialization failure.
    pub fn usage(ev: &UsageEvent) -> Result<Self, DomainError> {
        Self::json(QueueKind::Usage, ev.tenant_id, USAGE_PAYLOAD_TYPE, ev)
    }

    /// Audit event, partitioned by tenant.
    ///
    /// # Errors
    /// Payload larger than 64 KiB, or serialization failure.
    pub fn audit(ev: &AuditEvent) -> Result<Self, DomainError> {
        let tenant_id = match ev {
            AuditEvent::Turn(t) => t.tenant_id,
            AuditEvent::Mutation(m) => m.tenant_id,
        };
        Self::json(QueueKind::Audit, tenant_id, AUDIT_PAYLOAD_TYPE, ev)
    }

    /// Attachment cleanup, partitioned by tenant.
    ///
    /// # Errors
    /// Payload larger than 64 KiB, or serialization failure.
    pub fn attachment_cleanup(ev: &AttachmentCleanupEvent) -> Result<Self, DomainError> {
        Self::json(
            QueueKind::AttachmentCleanup,
            ev.tenant_id,
            ATTACHMENT_CLEANUP_PAYLOAD_TYPE,
            ev,
        )
    }

    /// Chat cleanup, partitioned by chat.
    ///
    /// # Errors
    /// Payload larger than 64 KiB, or serialization failure.
    pub fn chat_cleanup(ev: &ChatCleanupEvent) -> Result<Self, DomainError> {
        Self::json(
            QueueKind::ChatCleanup,
            ev.chat_id,
            CHAT_CLEANUP_PAYLOAD_TYPE,
            ev,
        )
    }

    /// Thread summary task, partitioned by chat.
    ///
    /// # Errors
    /// Payload larger than 64 KiB, or serialization failure.
    pub fn thread_summary(task: &ThreadSummaryTask) -> Result<Self, DomainError> {
        Self::json(
            QueueKind::ThreadSummary,
            task.chat_id,
            THREAD_SUMMARY_PAYLOAD_TYPE,
            task,
        )
    }
}

/// Writes outbox rows inside the caller's transaction.
///
/// Created with the services, attached to the platform [`Outbox`] once the pipeline has started
/// ([`Self::attach`]); enqueueing before that fails with `Internal("outbox not started")`.
pub struct OutboxEnqueuer {
    cfg: OutboxConfig,
    outbox: ArcSwapOption<Outbox>,
}

impl OutboxEnqueuer {
    #[must_use]
    pub fn new(cfg: &OutboxConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            outbox: ArcSwapOption::empty(),
        }
    }

    /// Connects the enqueuer to the started pipeline.
    pub fn attach(&self, outbox: Arc<Outbox>) {
        self.outbox.store(Some(outbox));
    }

    /// Writes `rec` through `tx`. The returned [`Wake`] must be fired after the transaction
    /// commits (hand it to `TxWakes::add` inside
    /// [`crate::infra::db::tx::write_tx_with_wakes`], which fires or discards it).
    ///
    /// # Errors
    /// `Internal("outbox not started")` before [`Self::attach`]; `Internal` on a database or
    /// outbox error.
    pub async fn enqueue(&self, tx: &DbTx<'_>, rec: OutboxRecord) -> Result<Wake, DomainError> {
        let outbox = self
            .outbox
            .load_full()
            .ok_or_else(|| DomainError::Internal("outbox not started".to_owned()))?;
        let queue = rec.kind.queue_name(&self.cfg);
        let partition = partition_for(rec.key, self.cfg.num_partitions);
        let record = Record::to(queue, partition)
            .payload(rec.payload, rec.payload_type)
            .build()
            .map_err(outbox_err)?;
        outbox.enqueue(tx, record).await.map_err(outbox_err)
    }
}

fn outbox_err(err: OutboxError) -> DomainError {
    match err {
        OutboxError::PayloadTooLarge { size, max } => {
            DomainError::OutboxPayloadTooLarge(format!("{size} bytes, the limit is {max}"))
        }
        other => DomainError::Internal(format!("outbox error: {other}")),
    }
}

/// One leased handler per queue.
#[derive(Clone)]
pub struct OutboxHandlers {
    pub usage: Arc<dyn LeasedMessageHandler>,
    pub attachment_cleanup: Arc<dyn LeasedMessageHandler>,
    pub chat_cleanup: Arc<dyn LeasedMessageHandler>,
    pub thread_summary: Arc<dyn LeasedMessageHandler>,
    pub audit: Arc<dyn LeasedMessageHandler>,
}

impl OutboxHandlers {
    /// [`LoggingAckHandler`] on every queue.
    #[must_use]
    pub fn logging() -> Self {
        let h: Arc<dyn LeasedMessageHandler> = Arc::new(LoggingAckHandler);
        Self {
            usage: Arc::clone(&h),
            attachment_cleanup: Arc::clone(&h),
            chat_cleanup: Arc::clone(&h),
            thread_summary: Arc::clone(&h),
            audit: h,
        }
    }

    /// Replaces the handler of `kind`.
    #[must_use]
    pub fn with(mut self, kind: QueueKind, handler: Arc<dyn LeasedMessageHandler>) -> Self {
        *self.slot(kind) = handler;
        self
    }

    /// Applies `f` to every handler (used by tests to wrap them).
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

    fn slot(&mut self, kind: QueueKind) -> &mut Arc<dyn LeasedMessageHandler> {
        match kind {
            QueueKind::Usage => &mut self.usage,
            QueueKind::AttachmentCleanup => &mut self.attachment_cleanup,
            QueueKind::ChatCleanup => &mut self.chat_cleanup,
            QueueKind::ThreadSummary => &mut self.thread_summary,
            QueueKind::Audit => &mut self.audit,
        }
    }
}

/// Placeholder handler: logs the payload type and acknowledges. Every queue has a real handler
/// (`usage`, `audit`, `attachment_cleanup`, `chat_cleanup`, `thread_summary`); tests use this one
/// to keep a queue quiet.
pub struct LoggingAckHandler;

#[async_trait]
impl LeasedMessageHandler for LoggingAckHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        tracing::info!(
            payload_type = %msg.payload_type,
            bytes = msg.payload.len(),
            "outbox message acknowledged without processing"
        );
        MessageResult::Ok
    }
}

/// Newtype that lets an `Arc<dyn LeasedMessageHandler>` be registered with the builder.
struct Delegating(Arc<dyn LeasedMessageHandler>);

#[async_trait]
impl LeasedMessageHandler for Delegating {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.0.handle(msg).await
    }
}

/// Registers the five queues and starts the pipeline.
///
/// Per-message attempts: `batch_size(1)` keeps the platform's retry counter per message, so a
/// handler's `attempts` cap applies to that message.
///
/// # Errors
/// Any [`OutboxError`] from the platform (e.g. a changed partition count).
pub async fn start_outbox(
    db: Db,
    cfg: &MiniChatConfig,
    handlers: OutboxHandlers,
) -> Result<OutboxHandle, OutboxError> {
    start_outbox_tuned(db, cfg, handlers, None).await
}

/// [`start_outbox`] with an optional idle interval for the sequencer, processors and reconciler
/// (tests poll fast instead of waiting for the platform's idle intervals).
///
/// # Errors
/// As [`start_outbox`].
pub async fn start_outbox_tuned(
    db: Db,
    cfg: &MiniChatConfig,
    handlers: OutboxHandlers,
    idle_interval: Option<Duration>,
) -> Result<OutboxHandle, OutboxError> {
    let q = &cfg.outbox;
    let partitions = Partitions::of(u16::try_from(q.num_partitions).unwrap_or(u16::MAX));
    let mut processor = WorkerTuning::processor_low_latency()
        .batch_size(1)
        .retry_max(RETRY_MAX);
    let mut builder = Outbox::builder(db).profile(OutboxProfile::low_latency());
    if let Some(idle) = idle_interval {
        processor = processor.idle_interval(idle);
        builder = builder
            .sequencer_tuning(WorkerTuning::sequencer_low_latency().idle_interval(idle))
            .reconciler_tuning(WorkerTuning::reconciler().idle_interval(idle));
    }
    builder
        .processor_tuning(processor)
        .queue(&q.queue_name, partitions)
        .leased(Delegating(handlers.usage))
        .queue(&q.cleanup_queue_name, partitions)
        .leased(Delegating(handlers.attachment_cleanup))
        .queue(&q.chat_cleanup_queue_name, partitions)
        .leased(Delegating(handlers.chat_cleanup))
        .queue(&q.thread_summary_queue_name, partitions)
        .leased(Delegating(handlers.thread_summary))
        .lease(LeaseConfig {
            duration: Duration::from_secs(cfg.thread_summary_worker.claim_timeout_secs),
            headroom: LEASE_HEADROOM,
        })
        .queue(&q.audit_queue_name, partitions)
        .leased(Delegating(handlers.audit))
        .lease(LeaseConfig {
            duration: AUDIT_LEASE,
            headroom: LEASE_HEADROOM,
        })
        .start()
        .await
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::AuditEvent;
    use uuid::Uuid;

    use super::*;
    use crate::domain::error::DomainError;
    use crate::infra::db::tx::write_tx_with_wakes;
    use crate::test_support::app::TestApp;
    use crate::test_support::fixtures::{mutation_audit_event, usage_event};

    #[test]
    fn partition_is_stable_and_in_range() {
        let id = Uuid::new_v4();
        for n in [1, 2, 4, 64] {
            assert!(partition_for(id, n) < n);
            assert_eq!(partition_for(id, n), partition_for(id, n));
        }
        // Last two bytes, big-endian.
        let id = Uuid::from_u128(0x0102);
        assert_eq!(partition_for(id, 64), 0x0102 % 64);
        assert_eq!(partition_for(Uuid::from_u128(7), 4), 3);
    }

    #[test]
    fn oversized_payload_rejected() {
        let mut ev = usage_event(Uuid::new_v4());
        ev.dedupe_key = "x".repeat(65 * 1024);
        let err = OutboxRecord::usage(&ev).unwrap_err();
        assert!(
            matches!(err, DomainError::OutboxPayloadTooLarge(_)),
            "{err:?}"
        );

        // Exactly at the limit still fits.
        let ok = OutboxRecord::new(QueueKind::Audit, Uuid::nil(), "t.v1", vec![0; 64 * 1024]);
        assert!(ok.is_ok());
        let err = OutboxRecord::new(
            QueueKind::ChatCleanup,
            Uuid::nil(),
            "t.v1",
            vec![0; 64 * 1024 + 1],
        )
        .unwrap_err();
        assert!(
            matches!(err, DomainError::ChatCleanupPayloadTooLarge(_)),
            "{err:?}"
        );
    }

    #[test]
    fn records_carry_payload_type_and_partition_key() {
        let tenant = Uuid::from_u128(11);
        let chat = Uuid::from_u128(12);
        let usage = OutboxRecord::usage(&usage_event(tenant)).unwrap();
        assert_eq!(
            (usage.kind, usage.key, usage.payload_type),
            (QueueKind::Usage, tenant, "mini-chat.usage.v1")
        );
        let audit = OutboxRecord::audit(&mutation_audit_event(tenant, Uuid::nil())).unwrap();
        assert_eq!(
            (audit.kind, audit.key, audit.payload_type),
            (QueueKind::Audit, tenant, "mini-chat.audit.v1")
        );
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let cleanup = OutboxRecord::attachment_cleanup(&AttachmentCleanupEvent {
            event_type: "attachment_deleted".to_owned(),
            tenant_id: tenant,
            chat_id: chat,
            attachment_id: Uuid::nil(),
            provider_file_id: None,
            vector_store_id: None,
            storage_backend: "openai".to_owned(),
            attachment_kind: "document".to_owned(),
            deleted_at: now,
            secondary_ref: None,
        })
        .unwrap();
        assert_eq!(
            (cleanup.kind, cleanup.key, cleanup.payload_type),
            (
                QueueKind::AttachmentCleanup,
                tenant,
                "mini-chat.attachment_cleanup.v1"
            )
        );
        let chat_cleanup = OutboxRecord::chat_cleanup(&ChatCleanupEvent {
            tenant_id: tenant,
            chat_id: chat,
            system_request_id: Uuid::nil(),
            reason: "chat_soft_delete".to_owned(),
            chat_deleted_at: now,
        })
        .unwrap();
        assert_eq!(
            (
                chat_cleanup.kind,
                chat_cleanup.key,
                chat_cleanup.payload_type
            ),
            (QueueKind::ChatCleanup, chat, "mini-chat.chat_cleanup.v1")
        );
        let summary = OutboxRecord::thread_summary(&ThreadSummaryTask {
            tenant_id: tenant,
            chat_id: chat,
            system_request_id: Uuid::nil(),
            base_frontier_created_at: None,
            base_frontier_message_id: None,
            frozen_target_created_at: now,
            frozen_target_message_id: Uuid::nil(),
            system_task_type: "thread_summary_update".to_owned(),
        })
        .unwrap();
        assert_eq!(
            (summary.kind, summary.key, summary.payload_type),
            (
                QueueKind::ThreadSummary,
                chat,
                "mini-chat.thread_summary.v1"
            )
        );
    }

    #[tokio::test]
    async fn enqueued_message_is_delivered_after_commit() {
        let app = TestApp::builder().build().await;
        let tenant = Uuid::new_v4();
        let (rolled_back, committed) = (Uuid::new_v4(), Uuid::new_v4());
        let queue = app.services.cfg.outbox.audit_queue_name.clone();

        // A rolled-back transaction delivers nothing.
        let outbox = Arc::clone(&app.services.outbox);
        let rec = OutboxRecord::audit(&mutation_audit_event(tenant, rolled_back)).unwrap();
        let res: Result<(), DomainError> =
            write_tx_with_wakes(&app.services.db, move |tx, wakes| {
                let (outbox, rec) = (Arc::clone(&outbox), rec.clone());
                Box::pin(async move {
                    wakes.add(outbox.enqueue(tx, rec).await?);
                    Err(DomainError::Internal("rollback".to_owned()))
                })
            })
            .await;
        assert!(res.is_err());

        let outbox = Arc::clone(&app.services.outbox);
        let rec = OutboxRecord::audit(&mutation_audit_event(tenant, committed)).unwrap();
        write_tx_with_wakes(&app.services.db, move |tx, wakes| {
            let (outbox, rec) = (Arc::clone(&outbox), rec.clone());
            Box::pin(async move {
                wakes.add(outbox.enqueue(tx, rec).await?);
                Ok(())
            })
        })
        .await
        .unwrap();

        TestApp::wait_until("the committed audit message is delivered", || async {
            !app.outbox_payloads(&queue).is_empty()
        })
        .await;
        // Same tenant, same partition: the rolled-back message would have come first.
        let payloads = app.outbox_payloads(&queue);
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        assert_eq!(payloads[0]["kind"], "mutation");
        assert_eq!(payloads[0]["request_id"], committed.to_string());
        let event: AuditEvent = serde_json::from_value(payloads[0].clone()).unwrap();
        assert!(matches!(event, AuditEvent::Mutation(_)));
        assert!(
            app.outbox_payloads(&app.services.cfg.outbox.queue_name)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn enqueue_before_the_pipeline_starts_is_an_internal_error() {
        let enqueuer = Arc::new(OutboxEnqueuer::new(&crate::config::OutboxConfig::default()));
        let test_db = crate::test_support::db::test_db().await;
        let provider = toolkit_db::DBProvider::<DomainError>::new(test_db.db());
        let rec = OutboxRecord::usage(&usage_event(Uuid::nil())).unwrap();
        let err = write_tx_with_wakes(&provider, |tx, wakes| {
            let (enqueuer, rec) = (Arc::clone(&enqueuer), rec.clone());
            Box::pin(async move {
                wakes.add(enqueuer.enqueue(tx, rec).await?);
                Ok(())
            })
        })
        .await
        .unwrap_err();
        assert!(
            matches!(&err, DomainError::Internal(m) if m == "outbox not started"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn injected_handler_replaces_the_default_and_deliveries_stay_recorded() {
        struct Rejecting(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl LeasedMessageHandler for Rejecting {
            async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                MessageResult::Reject("injected".to_owned())
            }
        }
        let handler = Arc::new(Rejecting(std::sync::atomic::AtomicUsize::new(0)));
        let app = TestApp::builder()
            .outbox_handler(QueueKind::Usage, handler.clone())
            .build()
            .await;
        let rec = OutboxRecord::usage(&usage_event(Uuid::new_v4())).unwrap();
        let outbox = Arc::clone(&app.services.outbox);
        write_tx_with_wakes(&app.services.db, move |tx, wakes| {
            let (outbox, rec) = (Arc::clone(&outbox), rec.clone());
            Box::pin(async move {
                wakes.add(outbox.enqueue(tx, rec).await?);
                Ok(())
            })
        })
        .await
        .unwrap();

        let queue = app.services.cfg.outbox.queue_name.clone();
        TestApp::wait_until("the injected handler ran", || async {
            handler.0.load(std::sync::atomic::Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(app.outbox_payloads(&queue).len(), 1);
    }
}
