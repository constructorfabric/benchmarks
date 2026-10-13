//! Usage event contract (DESIGN 3.2, 5.6, Appendix A.3).
//!
//! One [`UsageEvent`] is published to the model-policy plugin per settled
//! turn or system task. Delivery is at-least-once; consumers deduplicate on
//! [`UsageEvent::dedupe_key`].

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Terminal state of the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    /// The provider finished the response.
    Completed,
    /// The turn failed.
    Failed,
    /// The turn was cancelled (client disconnect).
    Cancelled,
}

/// Billing outcome of the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingOutcome {
    /// Billed on provider-reported usage.
    Completed,
    /// Failed after the provider started.
    Failed,
    /// Aborted before a terminal provider event.
    Aborted,
    /// Background system task.
    SystemTask,
}

/// How the quota was settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementMethod {
    /// Provider-reported usage.
    Actual,
    /// Deterministic estimate.
    Estimated,
    /// Reserve released without a debit.
    Released,
    /// No quota involved.
    None,
}

/// Who triggered the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequesterType {
    /// An end user.
    User,
    /// A background task.
    System,
}

/// Kind of a system task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemTaskType {
    /// Thread summary update.
    ThreadSummaryUpdate,
    /// Document summary generation (reserved).
    DocSummaryGeneration,
}

impl SystemTaskType {
    /// Wire name of the task type (used in dedupe keys).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThreadSummaryUpdate => "thread_summary_update",
            Self::DocSummaryGeneration => "doc_summary_generation",
        }
    }
}

/// Provider-reported token usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "field names are the pinned wire format"
)]
pub struct UsageTokens {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens served from the provider cache (subset of input).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Input tokens written to the provider cache (subset of input).
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    /// Reasoning tokens (subset of output).
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// Usage payload published through `publish_usage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Tenant of the turn.
    pub tenant_id: Uuid,
    /// User of the turn (omitted for system tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    /// Chat of the turn.
    pub chat_id: Uuid,
    /// Turn identifier (omitted for system tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    /// Request identifier of the turn or the system task.
    pub request_id: Uuid,
    /// Model that served the turn.
    pub effective_model: String,
    /// Model selected for the chat.
    pub selected_model: String,
    /// Terminal state.
    pub terminal_state: TerminalState,
    /// Billing outcome.
    pub billing_outcome: BillingOutcome,
    /// Provider usage; serialised as `null` when the provider reported none.
    pub usage: Option<UsageTokens>,
    /// Debited credits (micro).
    pub actual_credits_micro: i64,
    /// Settlement method.
    pub settlement_method: SettlementMethod,
    /// Policy version the turn was settled under.
    pub policy_version_applied: u64,
    /// Completed `web_search` calls.
    #[serde(default)]
    pub web_search_calls: u32,
    /// Completed `code_interpreter` calls.
    #[serde(default)]
    pub code_interpreter_calls: u32,
    /// Completed file-search calls.
    #[serde(default)]
    pub file_search_calls: u32,
    /// Event time.
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
    /// Requester type.
    pub requester_type: RequesterType,
    /// Stable idempotency key, see [`turn_dedupe_key`] and [`system_task_dedupe_key`].
    pub dedupe_key: String,
    /// Task type (omitted for user turns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_task_type: Option<SystemTaskType>,
}

/// Dedupe key of a user turn: `{tenant}/{turn}/{request}` in the simple
/// (32 lowercase hex, no hyphens) UUID form.
#[must_use]
pub fn turn_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        turn_id.as_simple(),
        request_id.as_simple()
    )
}

/// Dedupe key of a system task: `{tenant}/{task_type}/{request}` with simple UUIDs.
#[must_use]
pub fn system_task_dedupe_key(
    tenant_id: Uuid,
    task_type: SystemTaskType,
    request_id: Uuid,
) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.as_simple(),
        task_type.as_str(),
        request_id.as_simple()
    )
}
