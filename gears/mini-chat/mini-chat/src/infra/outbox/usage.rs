//! Usage outbox handler (`cpt-cf-mini-chat-algo-gear-foundation-handle-usage`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{PublishError, UsageEvent, decode_usage_event, encode_usage_event};
use toolkit_db::DbTx;
use toolkit_db::outbox::{
    LeasedMessageHandler, MessageResult, OutboxError, OutboxMessage, Record, Wake,
};

use crate::domain::error::DomainError;
use crate::domain::service::quota::events::{SystemTaskUsageArgs, build_system_task_event};
use crate::infra::outbox::queues::{OutboxEnqueuer, QueueKind};
use crate::infra::policy_gateway::ModelPolicyGateway;

/// Bound of `publish_usage`, shorter than the 30 s usage-queue lease.
pub const PUBLISH_USAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// Publishes usage events through the model-policy plugin.
pub struct UsageHandler {
    policy: Arc<ModelPolicyGateway>,
    publish_timeout: Duration,
}

impl UsageHandler {
    /// Creates the handler with the default `PUBLISH_USAGE_TIMEOUT`.
    #[must_use]
    pub fn new(policy: Arc<ModelPolicyGateway>) -> Self {
        Self {
            policy,
            publish_timeout: PUBLISH_USAGE_TIMEOUT,
        }
    }

    /// Overrides the bound of `publish_usage`.
    #[must_use]
    pub fn with_publish_timeout(mut self, timeout: Duration) -> Self {
        self.publish_timeout = timeout;
        self
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-decode-usage
        let decoded = decode_usage_event(&msg.payload);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-decode-usage
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-if-usage-malformed
        let event = match decoded {
            Ok(event) => event,
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-reject-malformed-usage
                return MessageResult::Reject(error.to_string());
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-reject-malformed-usage
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-if-usage-malformed
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-resolve-policy
        let client = match self.policy.client().await {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, "usage event delivery deferred: model policy plugin unavailable");
                return MessageResult::Retry;
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-resolve-policy
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-try-publish
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-publish
        let published =
            tokio::time::timeout(self.publish_timeout, client.publish_usage(event)).await;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-publish
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-try-publish
        match published {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-catch-publish-timeout
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-retry-publish-timeout
            Err(_elapsed) => {
                tracing::warn!("publish_usage timed out, the usage event is retried");
                MessageResult::Retry
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-retry-publish-timeout
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-catch-publish-timeout
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-catch-publish
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-classify-publish
            Ok(Err(PublishError::Transient(_))) => MessageResult::Retry,
            Ok(Err(PublishError::Permanent(reason))) => MessageResult::Reject(reason),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-classify-publish
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-catch-publish
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-return-usage-ok
            Ok(Ok(())) => MessageResult::Ok,
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-usage:p1:inst-return-usage-ok
        }
    }
}

/// Maps an outbox enqueue failure. A database-originated failure becomes
/// [`DomainError::Database`] with the driver text preserved, so the retry
/// classification of the quota transaction (`SQLITE_BUSY`, `40001`, `40P01`)
/// still sees a transient lock raised by the outbox insert; every other
/// rejection (unregistered queue, partition out of range, ...) is a defect and
/// stays [`DomainError::Internal`].
fn enqueue_error(error: OutboxError) -> DomainError {
    match error {
        OutboxError::Database(db_error) => DomainError::Database(db_error.to_string()),
        other => DomainError::Internal(format!("usage enqueue failed: {other}")),
    }
}

/// Encodes `event` and enqueues it to the usage queue (`outbox.queue_name`),
/// partitioned by `tenant_id`, inside the caller's transaction. The caller
/// fires the returned [`Wake`] after the commit.
///
/// # Errors
///
/// [`DomainError::Internal`] when the event cannot be encoded (a user event
/// without `user_id`), the outbox pipeline is not running or the outbox
/// rejects the message for a non-database reason; [`DomainError::Database`]
/// (driver text preserved, so contention is retryable) when the insert fails
/// in the database.
// @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-usage-events:p1
pub async fn enqueue_usage_event(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    event: &UsageEvent,
) -> Result<Wake, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-encode-turn-event
    let payload = encode_usage_event(event)
        .map_err(|error| DomainError::Internal(format!("usage event encoding failed: {error}")))?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-encode-turn-event
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-enqueue-turn-event
    let running = outbox
        .outbox()
        .ok_or_else(|| DomainError::Internal("the outbox pipeline is not running".to_owned()))?;
    let queue = outbox.queue_name(QueueKind::Usage).to_owned();
    let partition = outbox.partition(QueueKind::Usage, event.tenant_id, event.chat_id);
    let record = Record::to(&queue, partition)
        .payload(payload, "application/json")
        .build()
        .map_err(|error| DomainError::Internal(format!("usage enqueue failed: {error}")))?;
    let wake = running.enqueue(tx, record).await.map_err(enqueue_error)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-enqueue-turn-event
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-return-turn-event
    Ok(wake)
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-return-turn-event
}

/// The system-task usage helper of
/// `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`: builds the event of
/// a thread-summary commit and enqueues it in the caller's transaction (the one
/// that upserts the summary). Never reads or writes `quota_usage` or
/// `chat_turns`.
///
/// # Errors
///
/// See [`enqueue_usage_event`].
pub async fn enqueue_system_task_usage(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    args: &SystemTaskUsageArgs,
) -> Result<Wake, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-no-user-debit
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-enqueue-system-event
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-return-system-event
    enqueue_usage_event(tx, outbox, &build_system_task_event(args)).await
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-return-system-event
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-enqueue-system-event
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-no-user-debit
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "usage_enqueue_tests.rs"]
mod enqueue_tests;
