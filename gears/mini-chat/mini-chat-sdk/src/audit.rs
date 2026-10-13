//! Audit events delivered to the audit plugin through the audit outbox.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::usage::UsageTokens;

/// Tool-call counts of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision recorded in the turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QuotaPolicyDecision {
    /// `allow`, `downgrade` or `unknown` (orphan watchdog).
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PolicyDecisions {
    pub quota: QuotaPolicyDecision,
}

/// Turn finalization audit event (`turn_completed` / `turn_failed`).
///
/// `prompt`, `response`, `attachments`, `license` and `quota_scope` are empty
/// in P1 (no content is audited).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub requester_type: String,
    #[serde(default)]
    pub actor_user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: String,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub usage: Option<UsageTokens>,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    pub tool_calls: ToolCalls,
    pub policy_decisions: PolicyDecisions,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub quota_scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: time::OffsetDateTime,
}

/// Turn mutation audit event (`turn_retry`, `turn_edit`, `turn_delete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: time::OffsetDateTime,
}

/// Any audit event of the gear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "audit_kind", rename_all = "snake_case")]
pub enum MiniChatAuditEvent {
    Turn(TurnAuditEvent),
    TurnMutation(TurnMutationAuditEvent),
}

impl MiniChatAuditEvent {
    #[must_use]
    pub fn event_type(&self) -> &str {
        match self {
            Self::Turn(e) => &e.event_type,
            Self::TurnMutation(e) => &e.event_type,
        }
    }

    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::Turn(e) => e.tenant_id,
            Self::TurnMutation(e) => e.tenant_id,
        }
    }
}

/// Errors returned by audit plugins.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditPluginError {
    #[error("transient audit plugin error: {0}")]
    Transient(String),
    #[error("audit plugin timeout")]
    PluginTimeout,
    #[error("permanent audit plugin error: {0}")]
    Permanent(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_event_roundtrip() {
        let e = MiniChatAuditEvent::TurnMutation(TurnMutationAuditEvent {
            event_type: "turn_retry".into(),
            tenant_id: Uuid::nil(),
            actor_user_id: Uuid::nil(),
            chat_id: Uuid::nil(),
            original_request_id: Some(Uuid::nil()),
            new_request_id: Some(Uuid::max()),
            request_id: None,
            timestamp: time::OffsetDateTime::UNIX_EPOCH,
        });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["audit_kind"], "turn_mutation");
        assert!(v.get("request_id").is_none());
        let back: MiniChatAuditEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back.event_type(), "turn_retry");
    }
}
