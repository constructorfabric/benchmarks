//! Usage event construction (`cpt-cf-mini-chat-algo-quota-and-billing-usage-event`): the
//! `UsageEvent` payload of a settled turn, of a turn without a reserve and of a system task.
//! The enqueue itself is the [`UsageQueue`](crate::domain::quota::UsageQueue) port.

use mini_chat_sdk::{UsageEvent, UsageTokens};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::quota::billing::{BillingOutcome, SettlementMethod, TerminalState};

/// The `requester_type` of a user turn.
const REQUESTER_USER: &str = "user";

/// The `requester_type` of a system task.
const REQUESTER_SYSTEM: &str = "system";

/// The `system_task_type` of the thread-summary update.
const THREAD_SUMMARY_UPDATE: &str = "thread_summary_update";

/// The 32-character lowercase hexadecimal form of a UUID without hyphens or braces, used only
/// inside the `dedupe_key`; every UUID field of the JSON payload keeps the hyphenated form.
fn simple(id: Uuid) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-normalize
    id.simple().to_string()
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-normalize
}

/// `{tenant_id}/{turn_id}/{request_id}` in the normalized form: the tenant prefix check of the
/// outbox enqueuer holds by construction.
fn user_dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-dedupe-user
    format!(
        "{}/{}/{}",
        simple(tenant_id),
        simple(turn_id),
        simple(request_id)
    )
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-dedupe-user
}

/// The completed tool call counts of a turn.
#[domain_model]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCallCounts {
    pub web_search: u32,
    pub code_interpreter: u32,
    /// Reported in the usage event only; `quota_usage.file_search_calls` is never populated.
    pub file_search: u32,
}

/// The inputs of the usage event of a settled turn.
#[domain_model]
#[derive(Debug, Clone)]
pub struct TurnUsageEventInput<'a> {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    /// From `chat_turns.effective_model`: the authoritative model for billing reconciliation.
    pub effective_model: &'a str,
    /// From `chats.model` (the effective model for an orphan-finalized turn).
    pub selected_model: &'a str,
    pub terminal_state: TerminalState,
    pub billing_outcome: BillingOutcome,
    pub settlement_method: SettlementMethod,
    /// The provider-reported usage; reported only for an `actual` settlement.
    pub usage: Option<UsageTokens>,
    /// The committed credits: the authoritative billing amount (`actual_credits_micro`).
    pub committed_credits_micro: i64,
    pub policy_version_applied: u64,
    pub tool_counts: ToolCallCounts,
    pub timestamp: OffsetDateTime,
}

/// The usage event of a settled turn: `dedupe_key` is `{tenant_id}/{turn_id}/{request_id}`
/// (32-character lowercase hex without hyphens); `usage` is the provider usage for `actual`,
/// `None` for `estimated` and zero input and output tokens for `released`;
/// `requester_type` is `user`; the payload has no field outside [`UsageEvent`].
#[must_use]
pub fn build_turn_usage_event(input: &TurnUsageEventInput<'_>) -> UsageEvent {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-credits-usage
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-exclusions
    // `actual_credits_micro` is the committed credits (the authoritative amount, never
    // recomputed by the consumer); `usage` is the provider usage for `actual`, null for
    // `estimated` (the split is not emitted) and zero tokens for `released`. The payload type
    // has no reserve, error, decision or provider-identifier field and no `event_type`; the
    // caller passes the effective model as the selected model for an orphan-finalized turn.
    let usage = match input.settlement_method {
        SettlementMethod::Actual => input.usage,
        SettlementMethod::Estimated => None,
        SettlementMethod::Released => Some(UsageTokens::default()),
    };
    UsageEvent {
        tenant_id: input.tenant_id,
        user_id: Some(input.user_id),
        chat_id: input.chat_id,
        turn_id: Some(input.turn_id),
        request_id: input.request_id,
        effective_model: input.effective_model.to_owned(),
        selected_model: input.selected_model.to_owned(),
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-terminal-state
        // the internal terminal state, kept apart from the billing outcome (an orphan-finalized
        // turn is `failed` with the billing outcome `aborted`)
        terminal_state: input.terminal_state.as_str().to_owned(),
        billing_outcome: input.billing_outcome.as_str().to_owned(),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-terminal-state
        usage,
        actual_credits_micro: input.committed_credits_micro,
        settlement_method: input.settlement_method.as_str().to_owned(),
        policy_version_applied: input.policy_version_applied,
        web_search_calls: input.tool_counts.web_search,
        code_interpreter_calls: input.tool_counts.code_interpreter,
        file_search_calls: input.tool_counts.file_search,
        timestamp: input.timestamp,
        requester_type: REQUESTER_USER.to_owned(),
        dedupe_key: Some(user_dedupe_key(
            input.tenant_id,
            input.turn_id,
            input.request_id,
        )),
        system_task_type: None,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-exclusions
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-credits-usage
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-fields
}

/// The inputs of the placeholder event of a turn that holds no reserve.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct NoReserveUsageEventInput {
    pub tenant_id: Uuid,
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub terminal_state: TerminalState,
    pub tool_counts: ToolCallCounts,
    pub timestamp: OffsetDateTime,
}

/// The placeholder event: `aborted`, `estimated`, 0 credits, `usage` null, empty
/// `effective_model` and `selected_model`, `policy_version_applied` 0.
#[must_use]
pub fn build_no_reserve_usage_event(input: &NoReserveUsageEventInput) -> UsageEvent {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-no-reserve-event
    UsageEvent {
        tenant_id: input.tenant_id,
        user_id: input.user_id,
        chat_id: input.chat_id,
        turn_id: Some(input.turn_id),
        request_id: input.request_id,
        effective_model: String::new(),
        selected_model: String::new(),
        terminal_state: input.terminal_state.as_str().to_owned(),
        billing_outcome: BillingOutcome::Aborted.as_str().to_owned(),
        usage: None,
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::Estimated.as_str().to_owned(),
        policy_version_applied: 0,
        web_search_calls: input.tool_counts.web_search,
        code_interpreter_calls: input.tool_counts.code_interpreter,
        file_search_calls: input.tool_counts.file_search,
        timestamp: input.timestamp,
        requester_type: REQUESTER_USER.to_owned(),
        dedupe_key: Some(user_dedupe_key(
            input.tenant_id,
            input.turn_id,
            input.request_id,
        )),
        system_task_type: None,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement:p1:inst-final-no-reserve-event
}

/// The inputs of the usage event of a system task (the thread-summary update).
#[domain_model]
#[derive(Debug, Clone)]
pub struct SystemTaskUsageInput<'a> {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub summary_model: &'a str,
    pub usage: UsageTokens,
    /// The server-generated UUID v4 persisted at the enqueue of the durable work and reused
    /// unchanged on every retry.
    pub system_request_id: Uuid,
    pub timestamp: OffsetDateTime,
}

/// The system-task event: `requester_type` `system`, `billing_outcome` `system_task`,
/// `settlement_method` `none`, `terminal_state` `completed`, 0 credits, `policy_version_applied`
/// 0, the three call counts 0, `system_task_type` `thread_summary_update`, no `user_id` and no
/// `turn_id` (absent, not null), `dedupe_key` `{tenant_id}/{system_task_type}/{system_request_id}`.
#[must_use]
pub fn build_system_task_usage_event(input: &SystemTaskUsageInput<'_>) -> UsageEvent {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-system-task
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-system-fields
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-system-isolation
    // pure construction: no `chat_turns`, no `quota_usage` row and no CAS finalization is
    // touched, and the enqueue is left to the owner of the thread-summary worker
    UsageEvent {
        tenant_id: input.tenant_id,
        user_id: None,
        chat_id: input.chat_id,
        turn_id: None,
        request_id: input.system_request_id,
        effective_model: input.summary_model.to_owned(),
        selected_model: input.summary_model.to_owned(),
        terminal_state: TerminalState::Completed.as_str().to_owned(),
        billing_outcome: "system_task".to_owned(),
        usage: Some(input.usage),
        actual_credits_micro: 0,
        settlement_method: "none".to_owned(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: input.timestamp,
        requester_type: REQUESTER_SYSTEM.to_owned(),
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-dedupe-system
        dedupe_key: Some(format!(
            "{}/{THREAD_SUMMARY_UPDATE}/{}",
            simple(input.tenant_id),
            simple(input.system_request_id)
        )),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-dedupe-system
        system_task_type: Some(THREAD_SUMMARY_UPDATE.to_owned()),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-system-isolation
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-system-fields
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-event:p1:inst-event-system-task
}

#[cfg(test)]
#[path = "usage_event_tests.rs"]
mod usage_event_tests;
