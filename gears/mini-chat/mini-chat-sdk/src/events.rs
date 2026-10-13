//! Usage and audit events published through the mini-chat outbox
//! (DESIGN §5.6, §5.7, Appendix A.3, §3.9 audit events).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Provider-reported token usage (telemetry; credits use input/output only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[allow(clippy::struct_field_names)] // wire field names
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

/// Usage settlement event handed to the model-policy plugin's `publish_usage`.
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
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    /// RFC 3339 timestamp.
    pub timestamp: String,
    /// `user` | `system`
    pub requester_type: String,
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

/// Token usage carried by a turn audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[allow(clippy::struct_field_names)] // wire field names
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

/// Tool call counters of a turn (no code-interpreter count, DESIGN §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolCalls {
    pub web_search_calls: u32,
    pub file_search_calls: u32,
}

/// Quota decision recorded in a turn audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct QuotaPolicyDecision {
    /// `allow` | `downgrade` | `unknown`
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PolicyDecisions {
    pub quota: QuotaPolicyDecision,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub license: String,
    /// Empty in P1 (ADR-0009).
    #[serde(default)]
    pub quota_scope: String,
}

/// Audit event emitted on turn finalization (`turn_completed` / `turn_failed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnAuditEvent {
    /// `turn_completed` | `turn_failed`
    pub event_type: String,
    pub tenant_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// `user` | `system`
    pub requester_type: String,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    /// `completed` | `failed` | `cancelled`
    pub terminal_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub usage: AuditUsage,
    pub latency_ms: u64,
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
    pub timestamp: String,
}

/// Audit event emitted by a turn mutation (`turn_retry`, `turn_edit`, `turn_delete`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMutationAuditEvent {
    /// `turn_retry` | `turn_edit` | `turn_delete`
    pub event_type: String,
    pub tenant_id: Uuid,
    pub actor_user_id: Uuid,
    pub chat_id: Uuid,
    /// Original turn `request_id` (retry / edit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_request_id: Option<Uuid>,
    /// New turn `request_id` (retry / edit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_request_id: Option<Uuid>,
    /// Deleted turn `request_id` (delete).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    pub timestamp: String,
}

/// Any audit event carried by the `mini-chat.audit` outbox queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MiniChatAuditEvent {
    Turn(Box<TurnAuditEvent>),
    Mutation(TurnMutationAuditEvent),
}

impl MiniChatAuditEvent {
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
