//! Usage event published through the `mini-chat.usage_snapshot` outbox queue.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Provider-reported token usage (cache and reasoning counts are subsets).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_field_names, reason = "field names are the usage-event wire format")]
pub struct UsageTokens {
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Input tokens served from the provider cache.
    pub cache_read_input_tokens: i64,
    /// Input tokens written to the provider cache.
    pub cache_write_input_tokens: i64,
    /// Output tokens used for reasoning.
    pub reasoning_tokens: i64,
}

impl UsageTokens {
    /// `true` when input or output tokens are non-zero.
    #[must_use]
    pub fn is_known(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }
}

/// Settled usage of a user turn or a system task (DESIGN §5.6, A.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Requesting user; absent for system tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// Chat.
    pub chat_id: Uuid,
    /// Turn; absent for system tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    /// Turn request id or system request id.
    pub request_id: Uuid,
    /// Model actually used.
    pub effective_model: String,
    /// Model selected for the chat.
    pub selected_model: String,
    /// `completed`, `failed` or `cancelled`.
    pub terminal_state: String,
    /// `completed`, `failed`, `aborted` or `system_task`.
    pub billing_outcome: String,
    /// Provider-reported usage; `null` when unknown.
    pub usage: Option<UsageTokens>,
    /// Committed credits (authoritative debit).
    pub actual_credits_micro: i64,
    /// `actual`, `estimated`, `released` or `none`.
    pub settlement_method: String,
    /// Policy version applied at preflight.
    pub policy_version_applied: u64,
    /// Completed web search calls.
    pub web_search_calls: u32,
    /// Completed code interpreter calls.
    pub code_interpreter_calls: u32,
    /// File search calls.
    pub file_search_calls: u32,
    /// Emission time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// `user` or `system`.
    pub requester_type: String,
    /// Idempotency key.
    pub dedupe_key: String,
    /// System task type (system tasks only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<String>,
}
