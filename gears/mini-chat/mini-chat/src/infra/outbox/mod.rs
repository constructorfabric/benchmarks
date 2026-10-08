//! Outbox integration: payloads, the enqueuer adapter behind
//! [`OutboxPort`](crate::domain::ports::OutboxPort), the handlers and the
//! pipeline start ([`start_pipeline`]).
//!
//! Queues (all with `outbox.num_partitions` partitions):
//!
//! | queue (config key)            | payload                     | partition key |
//! |-------------------------------|-----------------------------|---------------|
//! | `queue_name` (usage)          | `mini_chat_sdk::UsageEvent` | `tenant_id`   |
//! | `audit_queue_name`            | `MiniChatAuditEvent`        | `tenant_id`   |
//! | `cleanup_queue_name`          | `AttachmentCleanupPayload`  | `tenant_id`   |
//! | `chat_cleanup_queue_name`     | `ChatCleanupPayload`        | `chat_id`     |
//! | `thread_summary_queue_name`   | `ThreadSummaryPayload`      | `chat_id`     |

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use toolkit_db::Db;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxBuilder, OutboxHandle,
    OutboxMessage, Partitions,
};
use uuid::Uuid;

use crate::config::{MiniChatConfig, OutboxConfig};
use crate::domain::ports::{AuditPort, PolicyPort, ThreadSummaryRunner};
use crate::domain::services::cleanup::CleanupService;
use crate::infra::metrics::MiniChatMetrics;

pub mod enqueuer;
pub mod handlers;
pub mod payloads;

use handlers::{
    AttachmentCleanupHandler, AuditHandler, ChatCleanupHandler, ThreadSummaryHandler, UsageHandler,
};

/// Lease of the audit queue (D B.9.3, hardcoded).
pub const AUDIT_LEASE: Duration = Duration::from_secs(60);

/// Payload type of every mini-chat outbox message.
pub const PAYLOAD_TYPE: &str = "application/json";

/// Stable partition of `key` in `0..num_partitions`: FNV-1a 64 over the 16
/// UUID bytes, modulo the partition count (`0` partitions maps to `0`).
#[must_use]
pub fn partition_for(key: Uuid, num_partitions: u32) -> u32 {
    if num_partitions == 0 {
        return 0;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // The remainder is < num_partitions (a u32), so the cast is lossless.
    #[allow(clippy::cast_possible_truncation)]
    {
        (hash % u64::from(num_partitions)) as u32
    }
}

/// Placeholder handler: leaves every message queued (`Retry`) until the real
/// handlers are registered by the pipeline builder.
#[derive(Debug, Clone, Copy, Default)]
pub struct RetryHandler;

#[async_trait]
impl LeasedMessageHandler for RetryHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        MessageResult::Retry
    }
}

/// Partition count for the builder; config validation guarantees a power of
/// two in `1..=64`, anything else falls back to the default 4.
fn partitions(cfg: &OutboxConfig) -> Partitions {
    match u16::try_from(cfg.num_partitions) {
        Ok(n) if (1..=64).contains(&n) && n.is_power_of_two() => Partitions::of(n),
        _ => Partitions::of(4),
    }
}

/// Register the five mini-chat queues on `builder` with [`RetryHandler`]
/// (no-op handlers, so tests can start a pipeline whose rows stay queued).
#[must_use]
pub fn register_queues(builder: OutboxBuilder, cfg: &OutboxConfig) -> OutboxBuilder {
    let parts = partitions(cfg);
    [
        &cfg.queue_name,
        &cfg.audit_queue_name,
        &cfg.cleanup_queue_name,
        &cfg.chat_cleanup_queue_name,
        &cfg.thread_summary_queue_name,
    ]
    .into_iter()
    .fold(builder, |b, name| {
        b.queue(name, parts).leased(RetryHandler).done()
    })
}

/// What the real handlers need.
pub struct HandlerDeps {
    /// Usage publication (model policy plugin).
    pub policy: Arc<dyn PolicyPort>,
    /// Audit delivery (audit gateway).
    pub audit: Arc<dyn AuditPort>,
    /// Attachment and chat cleanup.
    pub cleanup: Arc<CleanupService>,
    /// Thread summary tasks.
    pub thread_summary: Arc<dyn ThreadSummaryRunner>,
    /// Instruments of the audit delivery outcomes (`audit_emit`).
    pub metrics: Arc<MiniChatMetrics>,
}

/// Start the shared outbox pipeline with the five leased mini-chat queues
/// (`outbox.num_partitions` partitions each) and their real handlers:
/// usage, attachment cleanup and chat cleanup with the outbox default lease
/// (30 s), thread summary with `thread_summary_worker.claim_timeout_secs`,
/// audit with [`AUDIT_LEASE`].
///
/// # Errors
///
/// The outbox failed to start (e.g. the outbox tables are missing).
pub async fn start_pipeline(
    db: Db,
    cfg: &MiniChatConfig,
    deps: HandlerDeps,
) -> Result<OutboxHandle, anyhow::Error> {
    let o = &cfg.outbox;
    let parts = partitions(o);
    let summary_lease = LeaseConfig {
        duration: Duration::from_secs(cfg.thread_summary_worker.claim_timeout_secs),
        ..LeaseConfig::default()
    };
    let audit_lease = LeaseConfig {
        duration: AUDIT_LEASE,
        ..LeaseConfig::default()
    };
    Outbox::builder(db)
        .queue(&o.queue_name, parts)
        .leased(UsageHandler::new(deps.policy))
        .queue(&o.cleanup_queue_name, parts)
        .leased(AttachmentCleanupHandler::new(Arc::clone(&deps.cleanup)))
        .queue(&o.chat_cleanup_queue_name, parts)
        .leased(ChatCleanupHandler::new(deps.cleanup))
        .queue(&o.thread_summary_queue_name, parts)
        .leased(ThreadSummaryHandler::new(deps.thread_summary))
        .lease(summary_lease)
        .queue(&o.audit_queue_name, parts)
        .leased(AuditHandler::new(deps.audit).with_metrics(deps.metrics))
        .lease(audit_lease)
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("mini-chat outbox pipeline failed to start: {e}"))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;
