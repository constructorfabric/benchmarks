//! Audit events delivered through the `mini-chat.audit` outbox queue (ADR-0009).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::usage::UsageTokens;

/// Tool-call counts of a turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditToolCalls {
    /// Completed web search calls.
    pub web_search_calls: u32,
    /// File search calls (provider `file_search` plus `search_knowledge`).
    pub file_search_calls: u32,
}

/// Quota decision recorded in the audit event.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditQuotaDecision {
    /// `allow`, `downgrade` or `unknown` (orphan watchdog).
    pub decision: String,
    /// Model the downgrade started from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    /// Downgrade reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

/// Policy decisions of a turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPolicyDecisions {
    /// Quota decision.
    pub quota: AuditQuotaDecision,
    /// License decision (not populated in P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

/// Audit event of a finalized turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    /// `turn_completed` or `turn_failed`.
    pub event_type: String,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Requesting user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// Chat.
    pub chat_id: Uuid,
    /// Turn.
    pub turn_id: Uuid,
    /// Request id.
    pub request_id: Uuid,
    /// Selected model.
    pub selected_model: String,
    /// Effective model.
    pub effective_model: String,
    /// Token usage.
    #[serde(default)]
    pub usage: Option<UsageTokens>,
    /// Total latency in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Time to first token in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Tool-call counts.
    pub tool_calls: AuditToolCalls,
    /// Policy decisions.
    pub policy_decisions: AuditPolicyDecisions,
    /// Terminal error code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Prompt (empty in P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Response (empty in P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
    /// Attachment metadata (empty in P1).
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    /// Quota scope (empty in P1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_scope: Option<String>,
    /// Event time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event of a retry of the latest turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRetryAuditEvent {
    /// Tenant.
    pub tenant_id: Uuid,
    /// Acting user.
    pub actor_user_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Request id of the replaced turn.
    pub original_request_id: Uuid,
    /// Request id of the new turn.
    pub new_request_id: Uuid,
    /// Event time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event of an edit of the latest turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEditAuditEvent {
    /// Tenant.
    pub tenant_id: Uuid,
    /// Acting user.
    pub actor_user_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Request id of the replaced turn.
    pub original_request_id: Uuid,
    /// Request id of the new turn.
    pub new_request_id: Uuid,
    /// Event time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event of a delete of the latest turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnDeleteAuditEvent {
    /// Tenant.
    pub tenant_id: Uuid,
    /// Acting user.
    pub actor_user_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Request id of the deleted turn.
    pub request_id: Uuid,
    /// Event time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Envelope stored in the audit outbox queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant, reason = "serialized once per outbox message; boxing would not change the payload")]
pub enum MiniChatAuditEvent {
    /// `turn_completed` / `turn_failed`.
    Turn(TurnAuditEvent),
    /// `turn_retry`.
    TurnRetry(TurnRetryAuditEvent),
    /// `turn_edit`.
    TurnEdit(TurnEditAuditEvent),
    /// `turn_delete`.
    TurnDelete(TurnDeleteAuditEvent),
}

impl MiniChatAuditEvent {
    /// The `event_type` value of the event.
    #[must_use]
    pub fn event_type(&self) -> &str {
        match self {
            Self::Turn(e) => &e.event_type,
            Self::TurnRetry(_) => "turn_retry",
            Self::TurnEdit(_) => "turn_edit",
            Self::TurnDelete(_) => "turn_delete",
        }
    }
}
