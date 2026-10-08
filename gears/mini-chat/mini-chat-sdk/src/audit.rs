//! Audit event types delivered to the audit plugin through the
//! `mini-chat.audit` outbox queue (ADR-0009: content fields are empty in P1).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyMs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Tool call counts (there is no code interpreter count, DESIGN §4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaPolicyDecision {
    /// `allow`, `downgrade` or `unknown` (orphan watchdog).
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_scope: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    pub quota: QuotaPolicyDecision,
}

/// Audit event emitted when a turn is finalized
/// (`turn_completed` or `turn_failed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub tenant_id: Uuid,
    pub requester_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version_applied: Option<u64>,
    pub usage: AuditUsage,
    pub latency_ms: LatencyMs,
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Audit event emitted for a turn mutation
/// (`turn_retry`, `turn_edit`, `turn_delete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    /// Set for `turn_delete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    /// Set for `turn_retry` / `turn_edit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
}

/// Any audit event of the gear (the outbox payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AuditEvent {
    Turn(TurnAuditEvent),
    Mutation(TurnMutationAuditEvent),
}

impl AuditEvent {
    #[must_use]
    pub fn event_type(&self) -> &str {
        match self {
            Self::Turn(e) => &e.event_type,
            Self::Mutation(e) => &e.event_type,
        }
    }
}
