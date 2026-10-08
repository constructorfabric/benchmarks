//! Usage event published through the usage outbox to the model-policy plugin.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Provider-reported token usage (telemetry; credits are authoritative).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_field_names, reason = "wire field names")]
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Billing outcome values (`billing_outcome` field).
pub mod billing_outcome {
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    pub const ABORTED: &str = "aborted";
    pub const SYSTEM_TASK: &str = "system_task";
}

/// Settlement method values (`settlement_method` field).
pub mod settlement_method {
    pub const ACTUAL: &str = "actual";
    pub const ESTIMATED: &str = "estimated";
    pub const RELEASED: &str = "released";
    pub const NONE: &str = "none";
}

/// Requester type values.
pub mod requester_type {
    pub const USER: &str = "user";
    pub const SYSTEM: &str = "system";
}

/// System task type values.
pub mod system_task_type {
    pub const THREAD_SUMMARY_UPDATE: &str = "thread_summary_update";
    pub const DOC_SUMMARY_GENERATION: &str = "doc_summary_generation";
}

/// One usage settlement event (DESIGN Appendix A.3).
///
/// `user_id` and `turn_id` are omitted for system tasks; `system_task_type`
/// is omitted for user turns. `usage` is `null` when the provider reported no
/// usage. `actual_credits_micro` carries the committed (possibly capped)
/// credits and is the authoritative billing amount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    pub terminal_state: String,
    pub billing_outcome: String,
    pub usage: Option<UsageTokens>,
    pub actual_credits_micro: i64,
    pub settlement_method: String,
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    pub requester_type: String,
    pub dedupe_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

/// Canonical dedupe key of a user turn: `{tenant}/{turn}/{request}` in the
/// simple (32-char lowercase hex) UUID form.
#[must_use]
pub fn turn_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        turn_id.as_simple(),
        request_id.as_simple()
    )
}

/// Dedupe key of a system task: `{tenant}/{system_task_type}/{system_request_id}`.
#[must_use]
pub fn system_task_dedupe_key(tenant_id: Uuid, task_type: &str, system_request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        task_type,
        system_request_id.as_simple()
    )
}
