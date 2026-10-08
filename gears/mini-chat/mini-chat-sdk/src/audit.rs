//! Audit events delivered to the audit plugin (DESIGN §3.9, ADR-0009).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::models::UsageTokens;

/// Turn audit event types.
pub mod event_types {
    pub const TURN_COMPLETED: &str = "turn_completed";
    pub const TURN_FAILED: &str = "turn_failed";
    pub const TURN_RETRY: &str = "turn_retry";
    pub const TURN_EDIT: &str = "turn_edit";
    pub const TURN_DELETE: &str = "turn_delete";
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyMs {
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
}

/// Tool call counts (the audit type has no code-interpreter count).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaDecisionAudit {
    /// `allow`, `downgrade` or `unknown` (orphan watchdog path).
    pub decision: String,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisions {
    pub quota: QuotaDecisionAudit,
    /// Not populated in P1.
    pub license: Option<String>,
    /// Not populated in P1.
    pub quota_scope: Option<String>,
}

/// One structured event per finalized turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub tenant_id: Uuid,
    pub requester_type: String,
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub usage: UsageTokens,
    pub latency_ms: LatencyMs,
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    pub error_code: Option<String>,
    /// Empty in P1 (no content in audit events).
    pub prompt: String,
    /// Empty in P1.
    pub response: String,
    /// Empty in P1.
    pub attachments: Vec<serde_json::Value>,
    pub trace_id: Option<String>,
}

/// Retry / edit / delete audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: String,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    pub original_request_id: Option<Uuid>,
    pub new_request_id: Option<Uuid>,
    /// Target turn of a delete.
    pub request_id: Option<Uuid>,
    pub trace_id: Option<String>,
}

/// Envelope of everything sent to the audit plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // short-lived value, serialized once
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

    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::Turn(e) => e.tenant_id,
            Self::Mutation(e) => e.tenant_id,
        }
    }
}
