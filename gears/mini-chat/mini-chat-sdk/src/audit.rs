//! Audit event payloads emitted to the audit plugin.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::usage::UsageTokens;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaPolicyDecision {
    pub decision: String,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecisions {
    pub quota: QuotaPolicyDecision,
    pub license: Option<Value>,
    pub quota_scope: Option<String>,
}

/// Audit event for a finished turn (`turn_completed` | `turn_failed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub requester_type: String,
    pub actor_user_id: Option<Uuid>,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    pub error_code: Option<String>,
    pub usage: Option<UsageTokens>,
    pub latency_ms: Option<u64>,
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    pub prompt: Option<String>,
    pub response: Option<String>,
    pub attachments: Vec<Value>,
    pub trace_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Audit event for a turn mutation (`turn_retry` | `turn_edit` | `turn_delete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub actor_user_id: Uuid,
    pub original_request_id: Option<Uuid>,
    pub new_request_id: Option<Uuid>,
    pub request_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // payloads are moved once into the outbox; boxing would change the public API
pub enum MiniChatAuditEvent {
    Turn(TurnAuditEvent),
    Mutation(TurnMutationAuditEvent),
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod audit_tests;
