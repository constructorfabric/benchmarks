//! The thread-summary queue: the work item payload
//! (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`,
//! `inst-build-summary-payload`), its enqueue inside the finalization
//! transaction and the handler that replaces the foundation placeholder
//! (`cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use toolkit_db::DbTx;
use toolkit_db::outbox::{
    LeasedMessageHandler, MessageResult, OutboxError, OutboxMessage, Record, Wake,
};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::MessageBoundary;
use crate::domain::service::workers::HandlerOutcome;
use crate::domain::service::workers::summary::SummaryService;
use crate::domain::service::workers::summary_trigger::{SYSTEM_TASK_TYPE, SummaryTask};
use crate::infra::outbox::queues::{OutboxEnqueuer, QueueKind};

/// A payload that cannot be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed thread summary payload: {0}")]
pub struct MalformedSummaryPayload(pub String);

/// RFC 3339 UTC with exactly six fractional digits.
fn micros_text(at: OffsetDateTime) -> String {
    let at = at.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.microsecond()
    )
}

mod micros {
    use super::{OffsetDateTime, Rfc3339, UtcOffset, micros_text};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(at: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&micros_text(*at))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
        let text = String::deserialize(d)?;
        OffsetDateTime::parse(&text, &Rfc3339)
            .map(|at| at.to_offset(UtcOffset::UTC))
            .map_err(serde::de::Error::custom)
    }
}

mod micros_option {
    use super::{OffsetDateTime, Rfc3339, UtcOffset, micros_text};
    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(at: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error> {
        match at {
            Some(at) => s.serialize_str(&micros_text(*at)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|text| {
                OffsetDateTime::parse(&text, &Rfc3339)
                    .map(|at| at.to_offset(UtcOffset::UTC))
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    tenant_id: Uuid,
    chat_id: Uuid,
    system_request_id: Uuid,
    #[serde(with = "micros_option")]
    base_frontier_created_at: Option<OffsetDateTime>,
    base_frontier_message_id: Option<Uuid>,
    #[serde(with = "micros")]
    frozen_target_created_at: OffsetDateTime,
    frozen_target_message_id: Uuid,
    system_task_type: String,
}

/// The JSON object enqueued on `outbox.thread_summary_queue_name`: exactly the
/// keys `tenant_id`, `chat_id`, `system_request_id`, `base_frontier_created_at`
/// (RFC 3339 UTC with microseconds, or `null`), `base_frontier_message_id`
/// (UUID or `null`; both `null` or both set), `frozen_target_created_at`,
/// `frozen_target_message_id` and `system_task_type`.
///
/// # Errors
///
/// Serialization failure (not expected for this type).
pub fn encode_task(task: &SummaryTask) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&Wire {
        tenant_id: task.tenant_id,
        chat_id: task.chat_id,
        system_request_id: task.system_request_id,
        base_frontier_created_at: task.base.map(|base| base.created_at),
        base_frontier_message_id: task.base.map(|base| base.id),
        frozen_target_created_at: task.target.created_at,
        frozen_target_message_id: task.target.id,
        system_task_type: SYSTEM_TASK_TYPE.to_owned(),
    })
}

/// Decodes a payload for the handler.
///
/// # Errors
///
/// [`MalformedSummaryPayload`] for a missing or unknown key, an id that is not a
/// UUID, a timestamp that is not RFC 3339, only one half of the base pair
/// `null`, a `system_task_type` other than `thread_summary_update`, or not JSON
/// at all.
pub fn decode_task(bytes: &[u8]) -> Result<SummaryTask, MalformedSummaryPayload> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-decode-summary
    let wire: Wire = serde_json::from_slice(bytes)
        .map_err(|error| MalformedSummaryPayload(error.to_string()))?;
    if wire.system_task_type != SYSTEM_TASK_TYPE {
        return Err(MalformedSummaryPayload(format!(
            "system_task_type `{}`",
            wire.system_task_type
        )));
    }
    let base = match (wire.base_frontier_created_at, wire.base_frontier_message_id) {
        (Some(created_at), Some(id)) => Some(MessageBoundary { created_at, id }),
        (None, None) => None,
        _ => {
            return Err(MalformedSummaryPayload(
                "only one half of the base frontier is set".to_owned(),
            ));
        }
    };
    Ok(SummaryTask {
        tenant_id: wire.tenant_id,
        chat_id: wire.chat_id,
        system_request_id: wire.system_request_id,
        base,
        target: MessageBoundary {
            created_at: wire.frozen_target_created_at,
            id: wire.frozen_target_message_id,
        },
    })
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-decode-summary
}

/// Encodes `task` and enqueues it on the thread-summary queue, partitioned by
/// `chat_id`, inside the caller's transaction. The caller fires the returned
/// [`Wake`] after the commit.
///
/// # Errors
///
/// [`DomainError::Internal`] when the pipeline is not running or the queue is
/// not registered; [`DomainError::Database`] (driver text preserved, so
/// contention is retryable) when the insert fails.
pub async fn enqueue_summary_task(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    task: &SummaryTask,
) -> Result<Wake, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-enqueue-summary-task
    let payload = encode_task(task)
        .map_err(|error| DomainError::Internal(format!("summary task encoding failed: {error}")))?;
    let running = outbox
        .outbox()
        .ok_or_else(|| DomainError::Internal("the outbox pipeline is not running".to_owned()))?;
    let queue = outbox.queue_name(QueueKind::ThreadSummary).to_owned();
    let partition = outbox.partition(QueueKind::ThreadSummary, task.tenant_id, task.chat_id);
    let record = Record::to(&queue, partition)
        .payload(payload, "application/json")
        .build()
        .map_err(|error| DomainError::Internal(format!("summary enqueue failed: {error}")))?;
    running
        .enqueue(tx, record)
        .await
        .map_err(|error| match error {
            OutboxError::Database(db_error) => DomainError::Database(db_error.to_string()),
            other => DomainError::Internal(format!("summary enqueue failed: {other}")),
        })
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger:p1:inst-enqueue-summary-task
}

/// The handler of the thread-summary queue.
pub struct ThreadSummaryHandler {
    service: SummaryService,
}

impl ThreadSummaryHandler {
    /// Creates the handler over the summary logic.
    #[must_use]
    pub fn new(service: SummaryService) -> Self {
        Self { service }
    }
}

/// The outbox result of a handler outcome.
#[must_use]
pub fn message_result(outcome: HandlerOutcome) -> MessageResult {
    match outcome {
        HandlerOutcome::Ok => MessageResult::Ok,
        HandlerOutcome::Retry => MessageResult::Retry,
        HandlerOutcome::Reject(reason) => MessageResult::Reject(reason),
    }
}

// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-summary-handler:p1
#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let task = match decode_task(&msg.payload) {
            Ok(task) => task,
            Err(error) => {
                tracing::error!(partition = msg.partition_id, %error, "a malformed thread summary payload is rejected");
                return MessageResult::Reject(error.to_string());
            }
        };
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-summary-runs
        message_result(self.service.handle(&task, msg.attempts).await)
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-summary-runs
    }
}

#[cfg(test)]
#[path = "thread_summary_payload_tests.rs"]
mod payload_tests;

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod handler_tests;

#[cfg(test)]
#[path = "thread_summary_startup_tests.rs"]
mod startup_tests;
