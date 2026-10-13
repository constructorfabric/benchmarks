//! Usage and audit event payloads (DESIGN §5.6–5.9, Appendix A.3, §3.9 audit).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Provider-reported token usage (telemetry).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: i64,
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
}

/// Usage settlement event published through the usage outbox queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub tenant_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    pub request_id: Uuid,
    pub effective_model: String,
    pub selected_model: String,
    /// `completed` | `failed` | `cancelled`
    pub terminal_state: String,
    /// `completed` | `failed` | `aborted` | `system_task`
    pub billing_outcome: String,
    /// `null` when the provider reported no usage.
    pub usage: Option<UsageTokens>,
    pub actual_credits_micro: i64,
    /// `actual` | `estimated` | `released` | `none`
    pub settlement_method: String,
    pub policy_version_applied: i64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// `user` | `system`
    pub requester_type: String,
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

/// Tool call counters carried by the turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision recorded in the turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditQuotaDecision {
    /// `allow` | `downgrade` | `unknown`
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AuditPolicyDecisions {
    pub quota: AuditQuotaDecision,
    /// Not populated in P1 (ADR-0009).
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub quota_scope: Option<String>,
}

/// Turn finalization audit event (`turn_completed` | `turn_failed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    #[serde(default)]
    pub user_id: Option<Uuid>,
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
    pub latency_ms: u64,
    pub tool_calls: AuditToolCalls,
    pub policy_decisions: AuditPolicyDecisions,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

/// Turn mutation audit event (`turn_retry` | `turn_edit` | `turn_delete`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub timestamp: OffsetDateTime,
}

/// Any audit event delivered to the audit plugin.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn event(system: bool) -> UsageEvent {
        UsageEvent {
            tenant_id: Uuid::nil(),
            user_id: (!system).then(Uuid::nil),
            chat_id: Uuid::nil(),
            turn_id: (!system).then(Uuid::nil),
            request_id: Uuid::nil(),
            effective_model: "m".into(),
            selected_model: "m".into(),
            terminal_state: "completed".into(),
            billing_outcome: if system { "system_task".into() } else { "completed".into() },
            usage: None,
            actual_credits_micro: 0,
            settlement_method: "none".into(),
            policy_version_applied: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: OffsetDateTime::UNIX_EPOCH,
            requester_type: if system { "system".into() } else { "user".into() },
            dedupe_key: "k".into(),
            system_task_type: system.then(|| "thread_summary_update".into()),
        }
    }

    #[test]
    fn system_task_omits_user_and_turn() {
        let v = serde_json::to_value(event(true)).unwrap();
        assert!(v.get("user_id").is_none());
        assert!(v.get("turn_id").is_none());
        assert_eq!(v["system_task_type"], "thread_summary_update");
        assert!(v["usage"].is_null());
    }

    #[test]
    fn user_turn_omits_system_task_type() {
        let v = serde_json::to_value(event(false)).unwrap();
        assert!(v.get("system_task_type").is_none());
        assert!(v.get("user_id").is_some());
    }
}
