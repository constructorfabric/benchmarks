//! Shared outbox integration: the enqueuer used inside domain transactions
//! and the five leased queue handlers.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxError, OutboxHandle,
    OutboxMessage, Partitions, Record, Wake,
};
use toolkit_db::{Db, DbTx};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::{DomainError, DomainResult};

/// How long an enqueue waits for the pipeline to start (gear start runs
/// after the REST routes are served).
const READY_WAIT: Duration = Duration::from_secs(30);

/// Enqueues outbox records inside domain transactions. The pipeline is bound
/// once the gear has started it.
pub struct OutboxEnqueuer {
    outbox: OnceLock<Arc<Outbox>>,
    ready_tx: watch::Sender<bool>,
    partitions: u32,
    pub queues: OutboxConfig,
}

impl OutboxEnqueuer {
    #[must_use]
    pub fn new(cfg: &OutboxConfig) -> Self {
        let (ready_tx, _) = watch::channel(false);
        Self {
            outbox: OnceLock::new(),
            ready_tx,
            partitions: cfg.num_partitions.max(1),
            queues: cfg.clone(),
        }
    }

    /// Bind the started pipeline.
    pub fn bind(&self, outbox: &Arc<Outbox>) {
        if self.outbox.set(Arc::clone(outbox)).is_ok() {
            self.ready_tx.send_replace(true);
        }
    }

    /// `true` once the pipeline is bound.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.outbox.get().is_some()
    }

    async fn outbox(&self) -> DomainResult<Arc<Outbox>> {
        if let Some(o) = self.outbox.get() {
            return Ok(Arc::clone(o));
        }
        let mut rx = self.ready_tx.subscribe();
        let wait = tokio::time::timeout(READY_WAIT, rx.wait_for(|ready| *ready)).await;
        match (wait, self.outbox.get()) {
            (_, Some(o)) => Ok(Arc::clone(o)),
            _ => Err(DomainError::internal("outbox pipeline is not running")),
        }
    }

    /// Partition of a key (UUID bytes hashed onto the partition count).
    #[must_use]
    pub fn partition_of(&self, key: Uuid) -> u32 {
        let b = key.as_bytes();
        let v = u32::from_be_bytes([b[12], b[13], b[14], b[15]])
            ^ u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        v % self.partitions
    }

    /// Enqueue one record in the caller's transaction. The returned wake must
    /// be fired after commit.
    ///
    /// # Errors
    /// `OutboxPayloadTooLarge` for an oversized payload; `Internal` when the
    /// pipeline is not running or the insert fails.
    pub async fn enqueue(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        partition_key: Uuid,
        payload_type: &str,
        payload: Vec<u8>,
    ) -> DomainResult<Wake> {
        let outbox = self.outbox().await?;
        let record = Record::to(queue, self.partition_of(partition_key))
            .payload(payload, payload_type)
            .build()
            .map_err(map_outbox_error)?;
        outbox.enqueue(tx, record).await.map_err(map_outbox_error)
    }

    /// Serialize and enqueue a JSON payload.
    ///
    /// # Errors
    /// As [`Self::enqueue`].
    pub async fn enqueue_json<T: serde::Serialize + Sync>(
        &self,
        tx: &DbTx<'_>,
        queue: &str,
        partition_key: Uuid,
        payload_type: &str,
        payload: &T,
    ) -> DomainResult<Wake> {
        let bytes = serde_json::to_vec(payload)
            .map_err(|e| DomainError::internal(format!("outbox payload serialization: {e}")))?;
        self.enqueue(tx, queue, partition_key, payload_type, bytes)
            .await
    }
}

fn map_outbox_error(e: OutboxError) -> DomainError {
    match e {
        OutboxError::PayloadTooLarge { size, max } => DomainError::OutboxPayloadTooLarge {
            detail: format!("The cleanup payload is too large ({size} bytes, maximum {max})"),
        },
        OutboxError::Database(db) => DomainError::Database(db),
        other => DomainError::internal(format!("outbox enqueue: {other}")),
    }
}

/// Fire wakes after a successful commit.
pub fn fire(wakes: Vec<Wake>) {
    for w in wakes {
        w.fire();
    }
}

/// A queue handler adapter: the handler logic lives in the domain service.
#[async_trait]
pub trait QueueHandler: Send + Sync + 'static {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult;
}

struct Adapter<H: QueueHandler>(Arc<H>, &'static str);

#[async_trait]
impl<H: QueueHandler> LeasedMessageHandler for Adapter<H> {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let result = self.0.handle(msg).await;
        match &result {
            MessageResult::Ok => {}
            MessageResult::Retry => {
                tracing::debug!(
                    queue = self.1,
                    attempts = msg.attempts,
                    "mini-chat: outbox retry"
                );
            }
            MessageResult::Reject(reason) => {
                tracing::warn!(queue = self.1, %reason, "mini-chat: outbox message rejected");
            }
        }
        result
    }
}

/// The five handlers of the pipeline.
pub struct Handlers<U, A, C, S, Q>
where
    U: QueueHandler,
    A: QueueHandler,
    C: QueueHandler,
    S: QueueHandler,
    Q: QueueHandler,
{
    pub usage: Arc<U>,
    pub attachment_cleanup: Arc<A>,
    pub chat_cleanup: Arc<C>,
    pub thread_summary: Arc<S>,
    pub audit: Arc<Q>,
}

/// Start the pipeline with the five mini-chat queues and bind it.
///
/// # Errors
/// Outbox start failure.
pub async fn start<U, A, C, S, Q>(
    db: Db,
    enqueuer: &OutboxEnqueuer,
    handlers: Handlers<U, A, C, S, Q>,
    summary_lease: Duration,
) -> Result<OutboxHandle, OutboxError>
where
    U: QueueHandler,
    A: QueueHandler,
    C: QueueHandler,
    S: QueueHandler,
    Q: QueueHandler,
{
    let q = &enqueuer.queues;
    let n = u16::try_from(q.num_partitions).unwrap_or(4);
    let parts = || Partitions::of(n);
    let headroom = Duration::from_secs(2);
    let handle = Outbox::builder(db)
        .queue(&q.queue_name, parts())
        .leased(Adapter(handlers.usage, "usage"))
        .queue(&q.cleanup_queue_name, parts())
        .leased(Adapter(handlers.attachment_cleanup, "attachment_cleanup"))
        .queue(&q.chat_cleanup_queue_name, parts())
        .leased(Adapter(handlers.chat_cleanup, "chat_cleanup"))
        .queue(&q.thread_summary_queue_name, parts())
        .leased(Adapter(handlers.thread_summary, "thread_summary"))
        .lease(LeaseConfig {
            duration: summary_lease.max(headroom + Duration::from_secs(1)),
            headroom,
        })
        .queue(&q.audit_queue_name, parts())
        .leased(Adapter(handlers.audit, "audit"))
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom,
        })
        .start()
        .await?;
    enqueuer.bind(handle.outbox());
    Ok(handle)
}
