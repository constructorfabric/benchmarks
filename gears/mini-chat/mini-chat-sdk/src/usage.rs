//! Usage events published to the model-policy plugin (`publish_usage`).
//!
//! One event is enqueued to the usage outbox queue for every turn that took a
//! quota reserve (in the same transaction as the quota settlement), and one
//! for every committed thread summary (system task). Delivery is
//! at-least-once; consumers deduplicate by `dedupe_key`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Provider-reported token usage (telemetry). Cache and reasoning counts are
/// subsets of `input_tokens` / `output_tokens`, not additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
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

/// Serialized usage outbox payload.
///
/// String-typed outcome fields (not enums) keep the payload open for
/// downstream consumers:
/// - `billing_outcome`: `completed` | `failed` | `aborted` | `system_task`
/// - `settlement_method`: `actual` | `estimated` | `released` | `none`
/// - `terminal_state`: `completed` | `failed` | `cancelled`
/// - `requester_type`: `user` | `system`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub tenant_id: Uuid,
    /// Absent for system tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    /// Absent for system tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    pub request_id: Uuid,
    pub effective_model: String,
    pub selected_model: String,
    pub terminal_state: String,
    pub billing_outcome: String,
    /// `null` when the provider reported no usage.
    pub usage: Option<UsageTokens>,
    /// Authoritative billing debit (committed credits).
    pub actual_credits_micro: i64,
    pub settlement_method: String,
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: time::OffsetDateTime,
    pub requester_type: String,
    pub dedupe_key: String,
    /// Present for system tasks only (`thread_summary_update`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}

impl UsageEvent {
    /// Canonical dedupe key of a user turn: `{tenant}/{turn}/{request}` with
    /// every UUID in its 32-char lowercase hex (simple) form.
    #[must_use]
    pub fn turn_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
        format!(
            "{}/{}/{}",
            tenant_id.as_simple(),
            turn_id.as_simple(),
            request_id.as_simple()
        )
    }

    /// Canonical dedupe key of a system task:
    /// `{tenant}/{system_task_type}/{system_request_id}`.
    #[must_use]
    pub fn system_task_dedupe_key(
        tenant_id: Uuid,
        system_task_type: &str,
        system_request_id: Uuid,
    ) -> String {
        format!(
            "{}/{system_task_type}/{}",
            tenant_id.as_simple(),
            system_request_id.as_simple()
        )
    }
}
