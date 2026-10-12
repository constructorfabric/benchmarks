//! Thread-summary types: the frontier, the durable work item and the trigger.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::llm::InputRole;

/// The platform default subject id: the identity of system tasks (DESIGN "Provider Request
/// Metadata").
pub const SYSTEM_SUBJECT_ID: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);

/// `system_task_type` of the thread-summary work item and its usage event.
pub const SYSTEM_TASK_TYPE: &str = "thread_summary_update";

/// A position in the per-chat message order `(created_at ASC, id ASC)`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frontier {
    /// `messages.created_at` of the message.
    pub created_at: OffsetDateTime,
    /// `messages.id` of the message.
    pub message_id: Uuid,
}

impl PartialOrd for Frontier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Frontier {
    fn cmp(&self, other: &Self) -> Ordering {
        self.created_at
            .cmp(&other.created_at)
            .then_with(|| self.message_id.cmp(&other.message_id))
    }
}

/// The durable thread-summary work item (DESIGN "Durable scheduling"): the payload of the
/// `mini-chat.thread_summary` outbox message.
///
/// `system_request_id` is a server-generated UUID v4 that stays the same across every retry of
/// the message. The base frontier is absent when no summary existed at enqueue time.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryWorkItem {
    /// The tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat to summarize.
    pub chat_id: Uuid,
    /// Stable identity of the task.
    pub system_request_id: Uuid,
    /// `summarized_up_to_created_at` of the summary the task builds on.
    #[serde(default, with = "optional_time")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    /// `summarized_up_to_message_id` of the summary the task builds on.
    #[serde(default)]
    pub base_frontier_message_id: Option<Uuid>,
    /// `created_at` of the frozen target message.
    #[serde(with = "mini_chat_sdk::time_serde")]
    pub frozen_target_created_at: OffsetDateTime,
    /// `id` of the frozen target message.
    pub frozen_target_message_id: Uuid,
    /// Always [`SYSTEM_TASK_TYPE`].
    pub system_task_type: String,
}

impl SummaryWorkItem {
    /// A work item for the range `(base, target]` of a chat.
    #[must_use]
    pub fn new(
        tenant_id: Uuid,
        chat_id: Uuid,
        system_request_id: Uuid,
        base: Option<Frontier>,
        target: Frontier,
    ) -> Self {
        Self {
            tenant_id,
            chat_id,
            system_request_id,
            base_frontier_created_at: base.map(|frontier| frontier.created_at),
            base_frontier_message_id: base.map(|frontier| frontier.message_id),
            frozen_target_created_at: target.created_at,
            frozen_target_message_id: target.message_id,
            system_task_type: SYSTEM_TASK_TYPE.to_owned(),
        }
    }

    /// The base frontier; `None` when the task starts a new summary.
    #[must_use]
    pub fn base(&self) -> Option<Frontier> {
        match (self.base_frontier_created_at, self.base_frontier_message_id) {
            (Some(created_at), Some(message_id)) => Some(Frontier {
                created_at,
                message_id,
            }),
            _ => None,
        }
    }

    /// The frozen target frontier.
    #[must_use]
    pub fn target(&self) -> Frontier {
        Frontier {
            created_at: self.frozen_target_created_at,
            message_id: self.frozen_target_message_id,
        }
    }
}

/// `Option<OffsetDateTime>` as an RFC 3339 string or `null`.
mod optional_time {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    #[allow(
        clippy::ref_option,
        reason = "the signature is fixed by serde's `with` attribute"
    )]
    pub fn serialize<S: Serializer>(
        value: &Option<OffsetDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(value) => {
                let text = value.format(&Rfc3339).map_err(serde::ser::Error::custom)?;
                serializer.serialize_some(&text)
            }
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        let text = Option::<String>::deserialize(deserializer)?;
        text.map(|text| {
            OffsetDateTime::parse(&text, &Rfc3339).map_err(|_| {
                serde::de::Error::custom(format!("invalid RFC 3339 timestamp: {text}"))
            })
        })
        .transpose()
    }
}

/// What the context assembly of a completed turn says about the need for a summary. `Some` only
/// when at least one condition holds.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryTrigger {
    /// The assembled context reached `compression_threshold_pct` of the effective budget.
    pub proactive: bool,
    /// Context assembly dropped older messages.
    pub urgent: bool,
}

/// One message of the range a summary covers.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeMessage {
    /// Who wrote the message.
    pub role: InputRole,
    /// The stored text.
    pub content: String,
}

/// A stored summary: its text and frontier.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSummary {
    /// The summary text.
    pub text: String,
    /// The last message the summary represents.
    pub frontier: Frontier,
}

/// The atomic commit of a finished summary.
#[domain_model]
#[derive(Debug, Clone)]
pub struct CommitSummary {
    /// The chat.
    pub chat_id: Uuid,
    /// The frontier the task was based on; `None` when no summary existed.
    pub base: Option<Frontier>,
    /// The frozen target the summary advances the frontier to.
    pub target: Frontier,
    /// The new summary text.
    pub summary_text: String,
    /// The stored `token_estimate`.
    pub token_estimate: i32,
    /// The system-task usage event enqueued in the same transaction.
    pub usage_event: mini_chat_sdk::UsageEvent,
    /// The instant of the commit.
    pub now: OffsetDateTime,
}

/// The result of the commit transaction.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The summary is stored, the range compressed and the usage event enqueued.
    Committed,
    /// The target message was soft-deleted: nothing was written.
    FrontierDeleted,
    /// The summary the task was based on no longer exists: nothing was written.
    BaseMissing,
    /// Another commit already moved the frontier: nothing was written.
    CasConflict,
}

/// What a handler attempt asks the outbox to do with the message.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryOutcome {
    /// The task is finished (committed, superseded or moot).
    Done,
    /// The attempt failed; deliver the message again later.
    Retry,
    /// The task can never succeed; dead-letter it.
    Reject(String),
}
