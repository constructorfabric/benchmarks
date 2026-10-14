//! The five leased outbox queues and the transactional enqueuer.
//!
//! Queues: usage (`outbox.queue_name`), attachment cleanup, chat cleanup, thread summary
//! (lease `thread_summary_worker.claim_timeout_secs`) and audit (lease 60 s). All use
//! `outbox.num_partitions` partitions; usage and audit messages are partitioned by
//! `tenant_id`, chat cleanup and thread summary by `chat_id`. The handlers of attachment
//! cleanup, chat cleanup and thread summary belong to the features that own that behavior; the
//! audit handler belongs to the audit slice and the usage handler to quota and billing.

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::UsageEvent;
use time::format_description::well_known::Rfc3339;
use toolkit_db::Db;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxBuilder, OutboxHandle,
    OutboxMessage, Partitions, Record, Wake, WorkerTuning,
};
use toolkit_db::secure::{DBRunner, DbTx};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::chats::{CHAT_CLEANUP_REASON, ChatCleanupMessage, ChatCleanupQueue};
use crate::domain::error::{DomainError, EnqueueSite};
use crate::domain::quota::UsageQueue;
use crate::domain::streaming::TurnOutbox;
use crate::domain::streaming::summary::{THREAD_SUMMARY_UPDATE, ThreadSummaryWork};
use crate::infra::audit_gateway::AUDIT_PAYLOAD_TYPE;
use crate::infra::error_conv::outbox_error_for;

/// Hardcoded lease of the audit queue.
pub const AUDIT_LEASE: Duration = Duration::from_secs(60);

/// Payload type of a usage event message.
pub const USAGE_PAYLOAD_TYPE: &str = "mini_chat.usage_event";

/// Payload type of a chat-cleanup message.
pub const CHAT_CLEANUP_PAYLOAD_TYPE: &str = "mini_chat.chat_cleanup";

/// Payload type of a thread-summary work message.
pub const THREAD_SUMMARY_PAYLOAD_TYPE: &str = "mini_chat.thread_summary";

/// The system task type in the `dedupe_key` of a chat-cleanup message.
const CHAT_CLEANUP_TASK_TYPE: &str = "chat_cleanup";

/// The five queues of the gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueueKind {
    Usage,
    AttachmentCleanup,
    ChatCleanup,
    ThreadSummary,
    Audit,
}

/// What a queue is partitioned by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionBy {
    Tenant,
    Chat,
}

/// Registration data of one queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSpec {
    pub kind: QueueKind,
    pub name: String,
    pub partitions: u32,
    pub partition_by: PartitionBy,
    /// `None` keeps the shared outbox default lease (30 s).
    pub lease: Option<Duration>,
}

/// Why a `dedupe_key` was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DedupeKeyError {
    /// The key does not start with the 32-character lowercase hex of the tenant id and `/`.
    #[error("dedupe key must start with the 32-character lowercase hex tenant id and `/`")]
    BadTenantPrefix,
    /// The key has no component after the tenant prefix.
    #[error("dedupe key has no components after the tenant prefix")]
    MissingComponents,
}

/// The five queue registrations in order: usage, attachment cleanup, chat cleanup, thread
/// summary, audit.
#[must_use]
pub fn queue_specs(cfg: &OutboxConfig, thread_summary_claim_timeout_secs: u64) -> Vec<QueueSpec> {
    let spec = |kind, name: &str, partition_by, lease| QueueSpec {
        kind,
        name: name.to_owned(),
        partitions: cfg.num_partitions,
        partition_by,
        lease,
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-leases
    vec![
        spec(QueueKind::Usage, &cfg.queue_name, PartitionBy::Tenant, None),
        spec(
            QueueKind::AttachmentCleanup,
            &cfg.cleanup_queue_name,
            PartitionBy::Tenant,
            None,
        ),
        spec(
            QueueKind::ChatCleanup,
            &cfg.chat_cleanup_queue_name,
            PartitionBy::Chat,
            None,
        ),
        spec(
            QueueKind::ThreadSummary,
            &cfg.thread_summary_queue_name,
            PartitionBy::Chat,
            Some(Duration::from_secs(thread_summary_claim_timeout_secs)),
        ),
        spec(
            QueueKind::Audit,
            &cfg.audit_queue_name,
            PartitionBy::Tenant,
            Some(AUDIT_LEASE),
        ),
    ]
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-leases
}

/// The partition of a routing key: deterministic across processes and always below
/// `num_partitions`.
#[must_use]
pub fn partition_for(key: Uuid, num_partitions: u32) -> u32 {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-partition-keys
    // the tail bytes of a UUID are stable across processes and spread evenly
    let bytes = key.as_bytes();
    let tail = u32::from(u16::from_be_bytes([bytes[14], bytes[15]]));
    tail.checked_rem(num_partitions).unwrap_or(0)
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-partition-keys
}

/// 32-character lowercase hex form of a UUID, no hyphens.
#[must_use]
pub fn normalize_uuid_for_dedupe_key(id: Uuid) -> String {
    id.simple().to_string()
}

/// `{tenant_id}/{turn_id}/{request_id}`, every component in simple form.
#[must_use]
pub fn usage_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        normalize_uuid_for_dedupe_key(tenant_id),
        normalize_uuid_for_dedupe_key(turn_id),
        normalize_uuid_for_dedupe_key(request_id)
    )
}

/// `{tenant_id}/{system_task_type}/{system_request_id}`, UUIDs in simple form.
#[must_use]
pub fn system_task_dedupe_key(
    tenant_id: Uuid,
    system_task_type: &str,
    system_request_id: Uuid,
) -> String {
    format!(
        "{}/{system_task_type}/{}",
        normalize_uuid_for_dedupe_key(tenant_id),
        normalize_uuid_for_dedupe_key(system_request_id)
    )
}

/// Validate, before serialization, that a `dedupe_key` starts with the 32-character
/// lowercase hex form of `tenant_id` followed by `/`.
///
/// # Errors
///
/// [`DedupeKeyError`] when the prefix is wrong or no component follows it.
pub fn validate_dedupe_key(tenant_id: Uuid, dedupe_key: &str) -> Result<(), DedupeKeyError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-dedupe-validation
    let prefix = format!("{}/", normalize_uuid_for_dedupe_key(tenant_id));
    let Some(components) = dedupe_key.strip_prefix(&prefix) else {
        return Err(DedupeKeyError::BadTenantPrefix);
    };
    if components.is_empty() {
        return Err(DedupeKeyError::MissingComponents);
    }
    Ok(())
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-dedupe-validation
}

/// The handler slot of a queue whose owning feature has not wired its handler yet. A message
/// is never acknowledged, dropped or dead-lettered by it: it stays queued and is retried.
struct UnwiredHandler {
    queue: String,
}

#[async_trait]
impl LeasedMessageHandler for UnwiredHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-outcomes
        tracing::debug!(
            queue = %self.queue,
            attempts = msg.attempts,
            "no handler is wired for this queue yet; the message stays queued"
        );
        MessageResult::Retry
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-outcomes
    }
}

/// A handler owned elsewhere (the audit gateway), registered on its queue.
struct SharedHandler(Arc<dyn LeasedMessageHandler>);

#[async_trait]
impl LeasedMessageHandler for SharedHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.0.handle(msg).await
    }
}

/// Processor tuning of the shared outbox: one message per batch, because
/// `OutboxMessage::attempts` counts per partition and the audit handler bounds its retries at
/// the 120th attempt; the backoff is capped at 30 s (about an hour for 120 attempts).
fn processor_tuning() -> WorkerTuning {
    WorkerTuning::processor_default()
        .batch_size(1)
        .retry_max(Duration::from_secs(30))
}

/// The handlers the gear wires on its queues; a queue without a handler gets the slot of a
/// feature that has not wired its handler yet.
#[derive(Clone, Default)]
pub struct QueueHandlers {
    /// The audit gateway, on the audit queue.
    pub audit: Option<Arc<dyn LeasedMessageHandler>>,
    /// The usage handler of quota and billing, on the usage queue.
    pub usage: Option<Arc<dyn LeasedMessageHandler>>,
    /// The attachment cleanup handler of FEATURE 0005, on the attachment cleanup queue.
    pub attachment_cleanup: Option<Arc<dyn LeasedMessageHandler>>,
    /// The thread-summary handler of FEATURE 0007, on the thread-summary queue.
    pub thread_summary: Option<Arc<dyn LeasedMessageHandler>>,
    /// The chat-cleanup handler of FEATURE 0007, on the chat-cleanup queue.
    pub chat_cleanup: Option<Arc<dyn LeasedMessageHandler>>,
}

/// Register one queue with its leased handler on the builder: each queue is served by the handler
/// of its kind when the gear wired one (audit, usage, attachment cleanup, chat cleanup and thread
/// summary), and a queue without a handler gets the placeholder slot.
fn register_queue(
    builder: OutboxBuilder,
    spec: &QueueSpec,
    handlers: &QueueHandlers,
) -> anyhow::Result<OutboxBuilder> {
    let partitions = u16::try_from(spec.partitions)
        .ok()
        .filter(|n| (1..=64).contains(n) && n.is_power_of_two())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "queue `{}`: {} partitions is not a power of 2 within 1..=64",
                spec.name,
                spec.partitions
            )
        })?;
    let queue = builder.queue(&spec.name, Partitions::of(partitions));
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-register
    // the handler slot replaces the placeholder; the partitioning by `chat_id`, the leases and the
    // global processor tuning (batch 1, retry backoff capped at 30 s) stay as the foundation set
    // them
    let handler = match spec.kind {
        QueueKind::Audit => &handlers.audit,
        QueueKind::Usage => &handlers.usage,
        QueueKind::AttachmentCleanup => &handlers.attachment_cleanup,
        QueueKind::ChatCleanup => &handlers.chat_cleanup,
        QueueKind::ThreadSummary => &handlers.thread_summary,
    };
    let queue = match handler {
        Some(handler) => queue.leased(SharedHandler(Arc::clone(handler))),
        None => queue.leased(UnwiredHandler {
            queue: spec.name.clone(),
        }),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-handler-wiring:p1:inst-hw-register
    Ok(match spec.lease {
        Some(duration) => queue
            .lease(LeaseConfig {
                duration,
                ..LeaseConfig::default()
            })
            .done(),
        None => queue.done(),
    })
}

/// Register the five queues on the shared outbox and start the pipeline. The outbox
/// migrations must already have run.
///
/// # Errors
///
/// Any failure to register a queue or start the workers.
pub async fn start_pipeline(db: Db, specs: &[QueueSpec]) -> anyhow::Result<OutboxHandle> {
    start_pipeline_with_audit(db, specs, None).await
}

/// [`start_pipeline`] with the handler of the audit queue (the audit gateway); the other
/// queues keep their unwired slots until their features bring handlers.
///
/// # Errors
///
/// Any failure to register a queue or start the workers.
pub async fn start_pipeline_with_audit(
    db: Db,
    specs: &[QueueSpec],
    audit: Option<Arc<dyn LeasedMessageHandler>>,
) -> anyhow::Result<OutboxHandle> {
    start_pipeline_with_handlers(
        db,
        specs,
        &QueueHandlers {
            audit,
            ..QueueHandlers::default()
        },
    )
    .await
}

/// [`start_pipeline`] with the handlers of the audit and the usage queue; the other queues keep
/// their unwired slots until their features bring handlers.
///
/// # Errors
///
/// Any failure to register a queue or start the workers.
pub async fn start_pipeline_with_handlers(
    db: Db,
    specs: &[QueueSpec],
    handlers: &QueueHandlers,
) -> anyhow::Result<OutboxHandle> {
    let mut builder = Outbox::builder(db).processor_tuning(processor_tuning());
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-each-queue
    for spec in specs {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-register
        builder = register_queue(builder, spec, handlers)?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-register
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-each-queue
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-return
    Ok(builder.start().await?)
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-return
}

/// Transactional enqueuer, late-bound to the outbox started in `start` (REST routes are
/// registered before `start`, so services hold the enqueuer from `init`).
pub struct OutboxEnqueuer {
    specs: Vec<QueueSpec>,
    outbox: OnceLock<Weak<Outbox>>,
}

impl OutboxEnqueuer {
    /// Create an unbound enqueuer for the given queues.
    #[must_use]
    pub fn new(specs: Vec<QueueSpec>) -> Self {
        Self {
            specs,
            outbox: OnceLock::new(),
        }
    }

    /// Bind the started outbox. The enqueuer holds it weakly, so it never keeps a stopped
    /// pipeline alive; a second bind is ignored.
    pub fn bind(&self, outbox: &Arc<Outbox>) {
        if self.outbox.set(Arc::downgrade(outbox)).is_err() {
            tracing::warn!("the outbox is already bound to the enqueuer; keeping the first");
        }
    }

    /// Enqueue one raw message into `kind`'s queue inside the caller's transaction. The
    /// returned [`Wake`] must be fired after the commit.
    ///
    /// # Errors
    ///
    /// `OutboxPayloadTooLarge { site }` for an oversized payload; `Internal` when the outbox
    /// is not bound or the database fails.
    pub async fn enqueue_raw(
        &self,
        tx: &(impl DBRunner + Sync + ?Sized),
        kind: QueueKind,
        partition_key: Uuid,
        payload: Vec<u8>,
        payload_type: &str,
        site: EnqueueSite,
    ) -> Result<Wake, DomainError> {
        let spec = self
            .specs
            .iter()
            .find(|spec| spec.kind == kind)
            .ok_or_else(|| DomainError::internal(format!("queue {kind:?} is not configured")))?;
        let outbox = self
            .outbox
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| DomainError::internal("the outbox pipeline is not running"))?;
        let partition = partition_for(partition_key, spec.partitions);
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-payload-too-large
        let record = Record::to(&spec.name, partition)
            .payload(payload, payload_type)
            .build()
            .map_err(|e| outbox_error_for(e, site))?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-payload-too-large
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-transactional-enqueue
        // the message is written through the caller's transaction, so it commits or rolls back
        // with the domain write; the caller fires the wake after the commit
        outbox
            .enqueue(tx, record)
            .await
            .map_err(|e| outbox_error_for(e, site))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-outbox-queues:p1:inst-outbox-transactional-enqueue
    }

    /// Enqueue a usage event: validates the `dedupe_key` first, serializes the event as JSON
    /// and partitions by `tenant_id`.
    ///
    /// # Errors
    ///
    /// As [`enqueue_raw`](Self::enqueue_raw), plus `Internal` for an invalid dedupe key.
    pub async fn enqueue_usage(
        &self,
        tx: &(impl DBRunner + Sync + ?Sized),
        event: &UsageEvent,
        site: EnqueueSite,
    ) -> Result<Wake, DomainError> {
        if let Some(dedupe_key) = &event.dedupe_key {
            validate_dedupe_key(event.tenant_id, dedupe_key)
                .map_err(|e| DomainError::internal(format!("invalid usage dedupe key: {e}")))?;
        }
        let payload = serde_json::to_vec(event)
            .map_err(|e| DomainError::internal(format!("cannot serialize the usage event: {e}")))?;
        self.enqueue_raw(
            tx,
            QueueKind::Usage,
            event.tenant_id,
            payload,
            USAGE_PAYLOAD_TYPE,
            site,
        )
        .await
    }
}

/// The usage enqueue of the settlement transaction
/// (`cpt-cf-mini-chat-algo-quota-and-billing-usage-event`: `inst-event-enqueue`).
#[async_trait]
impl UsageQueue for OutboxEnqueuer {
    async fn enqueue_usage(&self, tx: &DbTx<'_>, event: &UsageEvent) -> Result<Wake, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-enqueue
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-missing-key
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-missing-key-fail
        // a quota-bearing event without a `dedupe_key` fails the transaction instead of
        // committing a message the consumer cannot deduplicate
        if event.dedupe_key.is_none() && event.system_task_type.is_none() {
            tracing::error!(
                tenant_id = %event.tenant_id,
                turn_id = ?event.turn_id,
                request_id = %event.request_id,
                "critical: a quota-bearing usage event has no dedupe key"
            );
            return Err(DomainError::internal(
                "a quota-bearing usage event has no dedupe key",
            ));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-missing-key-fail
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-missing-key
        // inside the caller's transaction, relying on the dedupe prefix validation of the
        // enqueuer; an oversized payload surfaces as `OutboxPayloadTooLarge` at the finalize site
        OutboxEnqueuer::enqueue_usage(self, tx, event, EnqueueSite::TurnFinalize).await
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-enqueue
    }
}

/// The audit and thread-summary enqueues of the turn finalization transactions (slice
/// S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`).
#[async_trait]
impl TurnOutbox for OutboxEnqueuer {
    async fn enqueue_turn_audit(
        &self,
        tx: &DbTx<'_>,
        event: &mini_chat_sdk::AuditEvent,
    ) -> Result<Wake, DomainError> {
        use mini_chat_sdk::AuditEvent;
        // the turn events are partitioned by `tenant_id`; the mutation events carry no tenant
        // and go through `enqueue_mutation_audit`, which is given the chat's tenant
        let partition_key = match event {
            AuditEvent::TurnCompleted(turn) | AuditEvent::TurnFailed(turn) => turn.tenant_id,
            AuditEvent::TurnRetry(_) | AuditEvent::TurnEdit(_) | AuditEvent::TurnDelete(_) => {
                return Err(DomainError::internal(
                    "a mutation event is enqueued through enqueue_mutation_audit",
                ));
            }
        };
        let payload = serde_json::to_vec(event)
            .map_err(|e| DomainError::internal(format!("cannot serialize the audit event: {e}")))?;
        // inside the caller's transaction, on the audit queue (`outbox.audit_queue_name`)
        self.enqueue_raw(
            tx,
            QueueKind::Audit,
            partition_key,
            payload,
            AUDIT_PAYLOAD_TYPE,
            EnqueueSite::TurnFinalize,
        )
        .await
    }

    async fn enqueue_mutation_audit(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        event: &mini_chat_sdk::AuditEvent,
    ) -> Result<Wake, DomainError> {
        use mini_chat_sdk::AuditEvent;
        let site = match event {
            AuditEvent::TurnRetry(_) => EnqueueSite::TurnRetry,
            AuditEvent::TurnEdit(_) => EnqueueSite::TurnEdit,
            AuditEvent::TurnDelete(_) => EnqueueSite::TurnDelete,
            AuditEvent::TurnCompleted(_) | AuditEvent::TurnFailed(_) => {
                return Err(DomainError::internal(
                    "a turn finalization event is not a mutation event",
                ));
            }
        };
        let payload = serde_json::to_vec(event)
            .map_err(|e| DomainError::internal(format!("cannot serialize the audit event: {e}")))?;
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-enqueue
        // inside the caller's transaction, on the audit queue, partitioned by the `tenant_id` of
        // the chat; an oversized payload is `OutboxPayloadTooLarge` at the mutation site (500)
        self.enqueue_raw(
            tx,
            QueueKind::Audit,
            tenant_id,
            payload,
            AUDIT_PAYLOAD_TYPE,
            site,
        )
        .await
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event:p1:inst-ae-enqueue
    }

    async fn enqueue_thread_summary(
        &self,
        tx: &DbTx<'_>,
        work: &ThreadSummaryWork,
    ) -> Result<Wake, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-enqueue
        let format = |instant: time::OffsetDateTime| {
            instant.format(&Rfc3339).map_err(|e| {
                DomainError::internal(format!("cannot format a summary frontier: {e}"))
            })
        };
        let base_created_at = work
            .base_frontier
            .map(|frontier| format(frontier.created_at))
            .transpose()?;
        let payload = serde_json::to_vec(&serde_json::json!({
            "tenant_id": work.tenant_id,
            "chat_id": work.chat_id,
            // generated once by the trigger and stable across every retry of the message
            "system_request_id": work.system_request_id,
            "system_task_type": THREAD_SUMMARY_UPDATE,
            "base_frontier_created_at": base_created_at,
            "base_frontier_message_id": work.base_frontier.map(|frontier| frontier.message_id),
            "frozen_target_created_at": format(work.frozen_target.created_at)?,
            "frozen_target_message_id": work.frozen_target.message_id,
            "dedupe_key": system_task_dedupe_key(
                work.tenant_id,
                THREAD_SUMMARY_UPDATE,
                work.system_request_id,
            ),
        }))
        .map_err(|e| {
            DomainError::internal(format!("cannot serialize the thread summary work: {e}"))
        })?;
        // inside the caller's transaction, on the thread-summary queue, partitioned by `chat_id`
        self.enqueue_raw(
            tx,
            QueueKind::ThreadSummary,
            work.chat_id,
            payload,
            THREAD_SUMMARY_PAYLOAD_TYPE,
            EnqueueSite::TurnFinalize,
        )
        .await
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-summary-trigger:p1:inst-sum-enqueue
    }
}

/// The chat-cleanup enqueue contract (`cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue`):
/// one message on the chat-cleanup queue, partitioned by `chat_id`, written in the caller's
/// transaction.
#[async_trait]
impl ChatCleanupQueue for OutboxEnqueuer {
    async fn enqueue_chat_cleanup(
        &self,
        tx: &DbTx<'_>,
        message: &ChatCleanupMessage,
    ) -> Result<Wake, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-request-id
        // generated once, here, and persisted in the payload: every retry, replay and
        // dead-letter handoff of this message reuses the serialized value unchanged
        let system_request_id = Uuid::new_v4();
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-request-id
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-payload
        let chat_deleted_at = message
            .chat_deleted_at
            .format(&Rfc3339)
            .map_err(|e| DomainError::internal(format!("cannot format chat_deleted_at: {e}")))?;
        let payload = serde_json::to_vec(&serde_json::json!({
            "tenant_id": message.tenant_id,
            "chat_id": message.chat_id,
            "system_request_id": system_request_id,
            "reason": CHAT_CLEANUP_REASON,
            "chat_deleted_at": chat_deleted_at,
            "dedupe_key": system_task_dedupe_key(
                message.tenant_id,
                CHAT_CLEANUP_TASK_TYPE,
                system_request_id,
            ),
        }))
        .map_err(|e| DomainError::internal(format!("cannot serialize the chat cleanup: {e}")))?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-payload
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-transactional
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-once
        // inside the caller's transaction, on the chat-cleanup queue, partitioned by `chat_id`;
        // the delete transaction calls this once, in the transaction that sets `deleted_at`, so
        // the message never exists for an active chat
        let enqueued = self
            .enqueue_raw(
                tx,
                QueueKind::ChatCleanup,
                message.chat_id,
                payload,
                CHAT_CLEANUP_PAYLOAD_TYPE,
                EnqueueSite::ChatDelete,
            )
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-once
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-transactional
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-too-large
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-return-error
        // a payload above the outbox limit is `OutboxPayloadTooLarge` at the chat-delete site,
        // which the delete flow maps to 400 `invalid_argument`
        let wake = enqueued?;
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-return-error
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-too-large
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-consumer-boundary
        // the message is only produced here: the chat-cleanup handler of background maintenance
        // consumes it and loads the attachment rows and the vector store row of the chat from
        // the `tenant_id` and `chat_id` of the payload
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-consumer-boundary
        // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-return
        Ok(wake)
        // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-cleanup-enqueue:p1:inst-enqueue-return
    }
}

#[cfg(test)]
#[path = "outbox_tests.rs"]
mod outbox_tests;

#[cfg(test)]
#[path = "chat_cleanup_tests.rs"]
mod chat_cleanup_tests;

#[cfg(test)]
#[path = "turn_outbox_tests.rs"]
mod turn_outbox_tests;

#[cfg(test)]
#[path = "mutation_outbox_tests.rs"]
mod mutation_outbox_tests;

#[cfg(test)]
#[path = "maintenance_wiring_tests.rs"]
mod maintenance_wiring_tests;
