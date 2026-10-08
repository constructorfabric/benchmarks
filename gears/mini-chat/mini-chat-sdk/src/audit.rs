//! Audit events delivered through the `mini-chat.audit` outbox queue to the
//! audit plugin (ADR-0009: P1 events carry no prompt/response content).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Turn audit event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAuditEventType {
    /// The turn finalized as `completed` (including a provider `incomplete`).
    TurnCompleted,
    /// Every other terminal state (failed, cancelled, orphan watchdog).
    TurnFailed,
}

/// Turn mutation audit event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names, reason = "wire event names (turn_retry, turn_edit, turn_delete)")]
pub enum TurnMutationEventType {
    TurnRetry,
    TurnEdit,
    TurnDelete,
}

/// Token usage carried by a turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_field_names, reason = "wire field names")]
pub struct AuditUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Latency metrics of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditLatency {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Tool-call counts of a turn (the audit type has no code interpreter count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision recorded for a turn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotaDecisionAudit {
    /// `allow`, `downgrade`, or `unknown` (orphan watchdog path).
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

/// Policy decisions recorded for a turn. `license` and `quota_scope` are
/// empty in P1.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyDecisions {
    pub quota: QuotaDecisionAudit,
    pub license: Option<String>,
    pub quota_scope: Option<String>,
}

/// Audit event of a finalized turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: TurnAuditEventType,
    pub tenant_id: Uuid,
    pub requester_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub usage: AuditUsage,
    pub latency: AuditLatency,
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub prompt: String,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub response: String,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event of a turn mutation (retry, edit, delete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: TurnMutationEventType,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    /// Retry/edit: the replaced turn's request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    /// Retry/edit: the new turn's request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    /// Delete: the deleted turn's request id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Envelope of every audit event delivered to the audit plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant, reason = "short-lived event envelope")]
pub enum AuditEvent {
    Turn(TurnAuditEvent),
    TurnMutation(TurnMutationAuditEvent),
}

impl AuditEvent {
    /// The `event_type` wire value.
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Turn(e) => match e.event_type {
                TurnAuditEventType::TurnCompleted => "turn_completed",
                TurnAuditEventType::TurnFailed => "turn_failed",
            },
            Self::TurnMutation(e) => match e.event_type {
                TurnMutationEventType::TurnRetry => "turn_retry",
                TurnMutationEventType::TurnEdit => "turn_edit",
                TurnMutationEventType::TurnDelete => "turn_delete",
            },
        }
    }

    /// Tenant of the event.
    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::Turn(e) => e.tenant_id,
            Self::TurnMutation(e) => e.tenant_id,
        }
    }
}
