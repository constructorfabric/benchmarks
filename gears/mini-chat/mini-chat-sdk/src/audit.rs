//! Audit events delivered to the audit plugin through the `mini-chat.audit`
//! outbox queue.
//!
//! P1 content scope: identities, model, token usage, latency, tool-call
//! counts and the quota decision. `prompt`, `response`, `attachments`,
//! `license` and `quota_scope` are always empty, so no redaction runs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `event_type` of a turn finalization audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAuditEventType {
    /// The turn was finalized as `completed` (including a provider
    /// `incomplete` response).
    TurnCompleted,
    /// Every other terminal state (failed, cancelled, orphan watchdog).
    TurnFailed,
}

/// Token usage carried by the turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub struct AuditUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
}

/// Latency metrics of the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditLatency {
    /// Time to first token in milliseconds (absent when no token arrived).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Tool-call counts of the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditToolCalls {
    pub web_search_calls: u32,
    /// Provider-native `file_search` calls plus `search_knowledge` retrievals.
    pub file_search_calls: u32,
}

/// Quota decision of the turn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditQuotaDecision {
    /// `allow`, `downgrade`, or `unknown` (orphan watchdog).
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    /// Not populated in P1.
    #[serde(default)]
    pub quota_scope: String,
}

/// Policy decisions of the turn.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditPolicyDecisions {
    /// Not populated in P1.
    #[serde(default)]
    pub license: String,
    pub quota: AuditQuotaDecision,
}

/// Audit event emitted once per finalized turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: TurnAuditEventType,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: time::OffsetDateTime,
    pub tenant_id: Uuid,
    pub requester_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<AuditUsage>,
    pub latency_ms: AuditLatency,
    pub tool_calls: AuditToolCalls,
    pub policy_decisions: AuditPolicyDecisions,
    /// OpenTelemetry trace id of the finalizing request (absent when no span
    /// is active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Not populated in P1 (no content in audit events).
    #[serde(default)]
    pub prompt: String,
    /// Not populated in P1.
    #[serde(default)]
    pub response: String,
    /// Not populated in P1.
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
}

/// Kind of a turn mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
pub enum TurnMutationKind {
    TurnRetry,
    TurnEdit,
    TurnDelete,
}

impl TurnMutationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnRetry => "turn_retry",
            Self::TurnEdit => "turn_edit",
            Self::TurnDelete => "turn_delete",
        }
    }
}

/// Audit event emitted by a retry, edit or delete of the last turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: TurnMutationKind,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: time::OffsetDateTime,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    /// `request_id` of the mutated turn (retry / edit: the original turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    /// `request_id` of the new turn (retry / edit only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    /// `request_id` of the deleted turn (delete only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
}
