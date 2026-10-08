//! Usage events published through the usage outbox (DESIGN §5.6, Appendix A.3).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Provider-reported token counts (cache and reasoning are subsets).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_field_names)] // wire contract (DESIGN usage event)
pub struct UsageTokens {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

impl UsageTokens {
    /// "Usage known" for failed turns: at least one non-zero count.
    #[must_use]
    pub fn is_nonzero(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }
}

/// Serialized payload of the usage queue. Strings (not enums) for the
/// outcome fields, per DESIGN §5.8 "Allowed outbox enum values".
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

/// `{tenant}/{turn}/{request}` with every component in simple (32-hex) form.
#[must_use]
pub fn turn_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        turn_id.as_simple(),
        request_id.as_simple()
    )
}

/// `{tenant}/{task_type}/{system_request_id}` for system tasks.
#[must_use]
pub fn system_task_dedupe_key(tenant_id: Uuid, task_type: &str, system_request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        task_type,
        system_request_id.as_simple()
    )
}
