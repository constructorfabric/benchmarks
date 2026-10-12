//! Thread summary (DESIGN 3.6 "Thread Summary Update", "Thread Summary - Stable
//! Range and Commit Invariant", B.5.5, B.9.3, B.9.4).
//!
//! * The trigger ([`SummaryTrigger`]) is decided from the facts of the context
//!   assembly of a turn; the stream service enqueues one durable
//!   [`ThreadSummaryTask`] in the finalization transaction of a completed turn
//!   that fires it.
//! * The work item is delivered to [`worker::ThreadSummaryService`] by the shared
//!   outbox: it summarizes exactly the range `(base, frozen target]`, calls the
//!   summary model and commits the result with a compare-and-set on the base
//!   frontier. It is a system task: no `chat_turns` row, no quota, no audit, a
//!   `system_task` usage event.
//!
//! The outbox adapters (`infra::outbox`) only translate [`SummaryOutcome`] into
//! the shared outbox's `MessageResult`.

mod prompt;
mod worker;

use mini_chat_sdk::{ModelCatalogEntry, SystemTaskType};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::config::ThreadSummaryWorkerConfig;
use crate::domain::error::DomainError;
use crate::domain::messages::SummaryFrontier;

pub use self::worker::{SummaryOutcome, ThreadSummaryService};

/// Built-in summary model: used when `thread_summary_worker.summary_model_id`
/// is empty (B.9.4).
const DEFAULT_SUMMARY_MODEL: &str = "gpt-4.1-mini";

/// Catalog id of the summary model: the configured one, else the built-in.
#[must_use]
pub fn summary_model_id(config: &ThreadSummaryWorkerConfig) -> &str {
    if config.summary_model_id.is_empty() {
        DEFAULT_SUMMARY_MODEL
    } else {
        config.summary_model_id.as_str()
    }
}

/// One durable thread-summary work item (the payload of the outbox message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummaryTask {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat to summarize.
    pub chat_id: Uuid,
    /// Stable identity of this work item; the usage event's `request_id`. It is
    /// part of the message, so it is the same on every retry and replay.
    pub system_request_id: Uuid,
    /// Frontier of the summary the work item builds on (`None`: no summary yet).
    pub base: Option<SummaryFrontier>,
    /// Inclusive upper bound of the range, frozen at enqueue time.
    pub target: SummaryFrontier,
}

/// The JSON of a work item (DESIGN 3.6 "Durable scheduling").
#[derive(Serialize, Deserialize)]
struct TaskWire {
    tenant_id: Uuid,
    chat_id: Uuid,
    system_request_id: Uuid,
    #[serde(default, with = "time::serde::rfc3339::option")]
    base_frontier_created_at: Option<OffsetDateTime>,
    #[serde(default)]
    base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    frozen_target_created_at: OffsetDateTime,
    frozen_target_message_id: Uuid,
    system_task_type: SystemTaskType,
}

impl ThreadSummaryTask {
    /// The outbox payload.
    ///
    /// # Errors
    /// `Internal` when the payload does not serialize.
    pub fn to_payload(&self) -> Result<Vec<u8>, DomainError> {
        serde_json::to_vec(&TaskWire {
            tenant_id: self.tenant_id,
            chat_id: self.chat_id,
            system_request_id: self.system_request_id,
            base_frontier_created_at: self.base.map(|base| base.created_at),
            base_frontier_message_id: self.base.map(|base| base.message_id),
            frozen_target_created_at: self.target.created_at,
            frozen_target_message_id: self.target.message_id,
            system_task_type: SystemTaskType::ThreadSummaryUpdate,
        })
        .map_err(|error| DomainError::internal(format!("thread summary payload: {error}")))
    }

    /// Reads a payload back.
    ///
    /// # Errors
    /// The reason when the bytes are not a thread-summary work item: not a JSON
    /// object, a field missing or of the wrong type, another `system_task_type`,
    /// or only one half of the base frontier.
    pub fn from_payload(payload: &[u8]) -> Result<Self, String> {
        let wire: TaskWire = serde_json::from_slice(payload).map_err(|error| error.to_string())?;
        if wire.system_task_type != SystemTaskType::ThreadSummaryUpdate {
            return Err(format!(
                "system_task_type `{}` is not a thread summary update",
                wire.system_task_type.as_str()
            ));
        }
        let base = match (wire.base_frontier_created_at, wire.base_frontier_message_id) {
            (Some(created_at), Some(message_id)) => Some(SummaryFrontier {
                created_at,
                message_id,
            }),
            (None, None) => None,
            _ => return Err("the base frontier is only half present".to_owned()),
        };
        Ok(Self {
            tenant_id: wire.tenant_id,
            chat_id: wire.chat_id,
            system_request_id: wire.system_request_id,
            base,
            target: SummaryFrontier {
                created_at: wire.frozen_target_created_at,
                message_id: wire.frozen_target_message_id,
            },
        })
    }
}

/// The stored summary of a chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRow {
    /// Row id.
    pub id: Uuid,
    /// Summary text (the column is nullable).
    pub text: Option<String>,
    /// Last message the summary covers.
    pub frontier: SummaryFrontier,
    /// Estimated size of the text in tokens.
    pub token_estimate: Option<i32>,
}

impl SummaryRow {
    /// The text that goes into the context: `None` when there is none.
    #[must_use]
    pub fn context_text(&self) -> Option<&str> {
        self.text.as_deref().filter(|text| !text.trim().is_empty())
    }

    /// `token_estimate`, or the byte estimate when the column is empty.
    #[must_use]
    pub fn tokens(&self) -> u64 {
        match self.token_estimate {
            Some(tokens) => u64::try_from(tokens).unwrap_or_default(),
            None => bytes_to_tokens(self.text.as_deref().unwrap_or_default()),
        }
    }
}

/// A summary to store (it replaces the chat's row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSummary {
    /// Row id.
    pub id: Uuid,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// Summary text.
    pub text: String,
    /// Last message the summary covers.
    pub frontier: SummaryFrontier,
    /// Estimated size of the text in tokens.
    pub token_estimate: i32,
    /// Creation time.
    pub now: OffsetDateTime,
}

/// `ceil(bytes / 4)`, the size estimate of a stored summary.
#[must_use]
pub fn bytes_to_tokens(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX).div_ceil(4)
}

/// `min(max_input_tokens, context_window - max_output_tokens_applied)` of the
/// effective model (`max_input_tokens = 0`: no separate limit); no tool
/// surcharge and no fixed overhead (DESIGN 3.6 "Summary trigger based on token
/// budget").
#[must_use]
pub fn effective_budget(entry: &ModelCatalogEntry, max_output_tokens_applied: u32) -> u64 {
    let room = u64::from(entry.context_window).saturating_sub(u64::from(max_output_tokens_applied));
    match entry.max_input_tokens {
        0 => room,
        limit => room.min(u64::from(limit)),
    }
}

/// What the context assembly of a turn tells the summary trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryTrigger {
    /// Frontier of the chat's summary read for the turn (`None`: no summary row).
    pub base: Option<SummaryFrontier>,
    /// Estimated size of the assembled request.
    pub assembled_tokens: u64,
    /// [`effective_budget`] of the effective model.
    pub effective_budget: u64,
    /// The context plan dropped at least one history message.
    pub truncated: bool,
}

impl SummaryTrigger {
    /// Proactive: no summary yet and the assembled context reached
    /// `threshold_pct` percent of the effective budget (inclusive). Urgent: the
    /// context plan dropped history. A chat that has a summary and no
    /// truncation never triggers, however large the context is.
    #[must_use]
    pub fn fires(&self, threshold_pct: u32) -> bool {
        if self.truncated {
            return true;
        }
        self.base.is_none()
            && u128::from(self.assembled_tokens).saturating_mul(100)
                >= u128::from(self.effective_budget).saturating_mul(u128::from(threshold_pct))
    }
}
