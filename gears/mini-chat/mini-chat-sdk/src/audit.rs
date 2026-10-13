//! Audit event types delivered to the audit plugin.
//!
//! In P1 the events carry identities, models, token usage, latency, tool-call counts and the
//! quota decision. `prompt`, `response`, `attachments`, `license` and `quota_scope` are empty,
//! so no content reaches the audit plugin.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::models::{RequesterType, TokenUsage, UsageTerminalState};

/// Event type of a turn-finalization audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnAuditEventType {
    /// The turn completed (including a provider `incomplete` response).
    #[serde(rename = "turn_completed")]
    Completed,
    /// Every other terminal state, including cancelled and orphan-finalized turns.
    #[serde(rename = "turn_failed")]
    Failed,
}

/// Event type of a turn-mutation audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnMutationEventType {
    /// The last turn was retried.
    #[serde(rename = "turn_retry")]
    Retry,
    /// The last turn was edited.
    #[serde(rename = "turn_edit")]
    Edit,
    /// The last turn was deleted.
    #[serde(rename = "turn_delete")]
    Delete,
}

/// Latency figures of a turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnLatency {
    /// Time to the first streamed token, when one was produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_to_first_token_ms: Option<u64>,
    /// Total turn duration.
    pub total_ms: u64,
}

/// Tool-call counts of a turn (no code interpreter count).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCalls {
    /// Web search calls.
    pub web_search_calls: u32,
    /// File search calls.
    pub file_search_calls: u32,
}

/// The quota decision taken for a turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaDecision {
    /// `allow`, `downgrade` or `unknown` (orphan-finalized turns).
    pub decision: String,
    /// Model the cascade started from, when a downgrade happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    /// Reason of the downgrade, when one happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

/// Policy decisions recorded with a turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisions {
    /// Quota decision.
    pub quota: QuotaDecision,
}

/// Audit event emitted when a turn reaches a terminal state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    /// Event type.
    pub event_type: TurnAuditEventType,
    /// Event time.
    #[serde(with = "crate::time_serde")]
    pub timestamp: OffsetDateTime,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Requester type.
    pub requester_type: RequesterType,
    /// Requesting user.
    pub user_id: Uuid,
    /// Chat of the turn.
    pub chat_id: Uuid,
    /// Turn identifier.
    pub turn_id: Uuid,
    /// Request identifier.
    pub request_id: Uuid,
    /// Model selected for the chat (the effective model on the orphan watchdog path).
    pub selected_model: String,
    /// Model actually used.
    pub effective_model: String,
    /// Terminal state of the turn.
    pub terminal_state: UsageTerminalState,
    /// Stored terminal error code, when the turn failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Provider-reported usage.
    #[serde(default)]
    pub usage: Option<TokenUsage>,
    /// Latency figures.
    #[serde(default)]
    pub latency: TurnLatency,
    /// Tool-call counts.
    #[serde(default)]
    pub tool_calls: ToolCalls,
    /// Policy decisions.
    #[serde(default)]
    pub policy_decisions: PolicyDecisions,
    /// Always empty in P1.
    #[serde(default)]
    pub prompt: String,
    /// Always empty in P1.
    #[serde(default)]
    pub response: String,
    /// OpenTelemetry trace id of the request that started the turn, omitted when no span was
    /// active (and on the orphan watchdog path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Fields of a retry or edit audit event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnReplacementAudit {
    /// Event time.
    #[serde(with = "crate::time_serde")]
    pub timestamp: OffsetDateTime,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// User who performed the mutation.
    pub actor_user_id: Uuid,
    /// Chat of the turn.
    pub chat_id: Uuid,
    /// Request of the mutated (original) turn.
    pub original_request_id: Uuid,
    /// Request of the replacement turn.
    pub new_request_id: Uuid,
}

/// Fields of a delete audit event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnDeleteAudit {
    /// Event time.
    #[serde(with = "crate::time_serde")]
    pub timestamp: OffsetDateTime,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// User who performed the mutation.
    pub actor_user_id: Uuid,
    /// Chat of the turn.
    pub chat_id: Uuid,
    /// Request of the deleted turn.
    pub request_id: Uuid,
}

/// Audit event emitted for a turn mutation, tagged on the wire by `event_type`.
///
/// Retry and edit carry `original_request_id` and `new_request_id`; delete carries `request_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_type")]
pub enum TurnMutationAuditEvent {
    /// The last turn was retried.
    #[serde(rename = "turn_retry")]
    Retry(TurnReplacementAudit),
    /// The last turn was edited.
    #[serde(rename = "turn_edit")]
    Edit(TurnReplacementAudit),
    /// The last turn was deleted.
    #[serde(rename = "turn_delete")]
    Delete(TurnDeleteAudit),
}

impl TurnMutationAuditEvent {
    /// The event type of this event.
    #[must_use]
    pub fn event_type(&self) -> TurnMutationEventType {
        match self {
            Self::Retry(_) => TurnMutationEventType::Retry,
            Self::Edit(_) => TurnMutationEventType::Edit,
            Self::Delete(_) => TurnMutationEventType::Delete,
        }
    }

    /// Owning tenant.
    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::Retry(e) | Self::Edit(e) => e.tenant_id,
            Self::Delete(e) => e.tenant_id,
        }
    }

    /// User who performed the mutation.
    #[must_use]
    pub fn actor_user_id(&self) -> Uuid {
        match self {
            Self::Retry(e) | Self::Edit(e) => e.actor_user_id,
            Self::Delete(e) => e.actor_user_id,
        }
    }

    /// Chat of the turn.
    #[must_use]
    pub fn chat_id(&self) -> Uuid {
        match self {
            Self::Retry(e) | Self::Edit(e) => e.chat_id,
            Self::Delete(e) => e.chat_id,
        }
    }

    /// Request of the mutated turn (the original request for retry and edit).
    #[must_use]
    pub fn mutated_request_id(&self) -> Uuid {
        match self {
            Self::Retry(e) | Self::Edit(e) => e.original_request_id,
            Self::Delete(e) => e.request_id,
        }
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
