//! Audit events delivered to the audit plugin.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::usage::UsageTokens;

/// Turn finalization event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnAuditEventType {
    TurnCompleted,
    TurnFailed,
}

/// Tool-call counts of a turn (no code interpreter count).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision (`allow`, `downgrade` or `unknown`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditQuotaDecision {
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

/// Policy decisions taken for a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditPolicyDecisions {
    pub quota: AuditQuotaDecision,
}

/// Audit event emitted on turn finalization.
///
/// `prompt`, `response`, `attachments`, `license` and `quota_scope` are empty
/// in P1 (ADR-0009). `timestamp` is an RFC 3339 string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: TurnAuditEventType,
    pub tenant_id: Uuid,
    /// The requester.
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub usage: UsageTokens,
    pub latency_ms: u64,
    pub tool_calls: AuditToolCalls,
    pub policy_decisions: AuditPolicyDecisions,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    #[serde(default)]
    pub license: Option<serde_json::Value>,
    #[serde(default)]
    pub quota_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub timestamp: String,
}

/// Audit event emitted for a turn mutation (retry, edit, delete).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type")]
pub enum TurnMutationAuditEvent {
    #[serde(rename = "turn_retry")]
    Retry {
        tenant_id: Uuid,
        actor_user_id: Uuid,
        chat_id: Uuid,
        original_request_id: Uuid,
        new_request_id: Uuid,
        timestamp: String,
    },
    #[serde(rename = "turn_edit")]
    Edit {
        tenant_id: Uuid,
        actor_user_id: Uuid,
        chat_id: Uuid,
        original_request_id: Uuid,
        new_request_id: Uuid,
        timestamp: String,
    },
    #[serde(rename = "turn_delete")]
    Delete {
        tenant_id: Uuid,
        actor_user_id: Uuid,
        chat_id: Uuid,
        request_id: Uuid,
        timestamp: String,
    },
}

impl TurnMutationAuditEvent {
    /// Tenant the event belongs to.
    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::Retry { tenant_id, .. }
            | Self::Edit { tenant_id, .. }
            | Self::Delete { tenant_id, .. } => *tenant_id,
        }
    }
}

/// Any audit event; serialized flat, each variant carrying its own
/// `event_type` (`turn_completed`, `turn_failed`, `turn_retry`, `turn_edit`,
/// `turn_delete`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MiniChatAuditEvent {
    Turn(TurnAuditEvent),
    Mutation(TurnMutationAuditEvent),
}

impl MiniChatAuditEvent {
    /// Tenant the event belongs to.
    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::Turn(e) => e.tenant_id,
            Self::Mutation(e) => e.tenant_id(),
        }
    }
}
