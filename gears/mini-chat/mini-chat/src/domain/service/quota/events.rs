//! Usage events of the quota service
//! (`cpt-cf-mini-chat-algo-quota-billing-turn-usage-event`,
//! `cpt-cf-mini-chat-algo-quota-billing-system-task-usage`,
//! `cpt-cf-mini-chat-dod-quota-billing-usage-events`): pure construction of
//! the SDK `UsageEvent`. Encoding and enqueueing are done by the infrastructure
//! in the caller's transaction.

use mini_chat_sdk::{
    BillingOutcome, RequesterType, SettlementMethod, SystemTaskType, TerminalState, UsageEvent,
    UsageTokens, system_task_dedupe_key, usage_dedupe_key,
};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::settle::ToolCallCounts;

/// Inputs of the event of one finalized user turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct TurnUsageEventArgs {
    /// `chat_turns.tenant_id`.
    pub tenant_id: Uuid,
    /// `chat_turns.requester_user_id` (never NULL here).
    pub user_id: Uuid,
    /// `chat_turns.chat_id`.
    pub chat_id: Uuid,
    /// `chat_turns.id`.
    pub turn_id: Uuid,
    /// `chat_turns.request_id`.
    pub request_id: Uuid,
    /// `chat_turns.effective_model`; empty for a turn without reserve.
    pub effective_model: String,
    /// The selected model; empty for a turn without reserve.
    pub selected_model: String,
    /// Terminal state.
    pub terminal_state: TerminalState,
    /// Billing outcome.
    pub billing_outcome: BillingOutcome,
    /// Settlement method applied.
    pub settlement_method: SettlementMethod,
    /// The committed credits (`actual_credits_micro` of the event).
    pub committed_credits_micro: i64,
    /// `chat_turns.policy_version_applied`; 0 for a turn without reserve.
    pub policy_version_applied: u64,
    /// Provider usage as received (the method decides how it is reported).
    pub usage: Option<UsageTokens>,
    /// Completed tool calls.
    pub tool_calls: ToolCallCounts,
    /// Event time.
    pub now: OffsetDateTime,
}

/// The `usage` field of a turn event: on `actual` the provider usage (`None`
/// when the turn received no usage object), on `estimated` `None`, on
/// `released` all five fields 0.
#[must_use]
pub fn event_usage(method: SettlementMethod, usage: Option<&UsageTokens>) -> Option<UsageTokens> {
    match method {
        SettlementMethod::Actual => usage.copied(),
        SettlementMethod::Released => Some(UsageTokens {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
        }),
        SettlementMethod::Estimated | SettlementMethod::None => None,
    }
}

/// Builds the turn usage event: `requester_type` `user`, no
/// `system_task_type`, `dedupe_key` `{tenant hex}/{turn hex}/{request hex}`
/// (32-character lowercase simple hex), `timestamp` = `now`.
#[must_use]
pub fn build_turn_usage_event(args: &TurnUsageEventArgs) -> UsageEvent {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-build-turn-event
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-excluded-fields
    UsageEvent {
        tenant_id: args.tenant_id,
        user_id: Some(args.user_id),
        chat_id: args.chat_id,
        turn_id: Some(args.turn_id),
        request_id: args.request_id,
        effective_model: args.effective_model.clone(),
        selected_model: args.selected_model.clone(),
        terminal_state: args.terminal_state,
        billing_outcome: args.billing_outcome,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-set-usage
        usage: event_usage(args.settlement_method, args.usage.as_ref()),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-set-usage
        actual_credits_micro: args.committed_credits_micro,
        settlement_method: args.settlement_method,
        policy_version_applied: args.policy_version_applied,
        web_search_calls: args.tool_calls.web_search,
        code_interpreter_calls: args.tool_calls.code_interpreter,
        file_search_calls: args.tool_calls.file_search,
        timestamp: args.now,
        requester_type: RequesterType::User,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-dedupe-key
        dedupe_key: usage_dedupe_key(args.tenant_id, args.turn_id, args.request_id),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-dedupe-key
        system_task_type: None,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-excluded-fields
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-turn-usage-event:p1:inst-build-turn-event
}

/// Inputs of the event of one thread-summary commit.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct SystemTaskUsageArgs {
    /// `chats.tenant_id`.
    pub tenant_id: Uuid,
    /// The summarized chat.
    pub chat_id: Uuid,
    /// The stable `system_request_id` of the thread-summary outbox payload.
    pub system_request_id: Uuid,
    /// The summary model id.
    pub summary_model: String,
    /// Provider token counts, or `None` when none were reported.
    pub usage: Option<UsageTokens>,
    /// Event time.
    pub now: OffsetDateTime,
}

/// Builds the system-task usage event: `requester_type` `system`,
/// `system_task_type` `thread_summary_update`, `completed` / `system_task` /
/// `none`, 0 credits, `policy_version_applied` 0, no `user_id` and `turn_id`,
/// all tool counters 0, `dedupe_key`
/// `{tenant hex}/thread_summary_update/{system request hex}`.
#[must_use]
pub fn build_system_task_event(args: &SystemTaskUsageArgs) -> UsageEvent {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-build-system-event
    UsageEvent {
        tenant_id: args.tenant_id,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-omit-user-turn
        user_id: None,
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-omit-user-turn
        chat_id: args.chat_id,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-omit-user-turn
        turn_id: None,
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-omit-user-turn
        request_id: args.system_request_id,
        effective_model: args.summary_model.clone(),
        selected_model: args.summary_model.clone(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::SystemTask,
        usage: args.usage,
        actual_credits_micro: 0,
        settlement_method: SettlementMethod::None,
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: args.now,
        requester_type: RequesterType::System,
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-system-dedupe-key
        dedupe_key: system_task_dedupe_key(
            args.tenant_id,
            SystemTaskType::ThreadSummaryUpdate,
            args.system_request_id,
        ),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-system-dedupe-key
        system_task_type: Some(SystemTaskType::ThreadSummaryUpdate),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-system-task-usage:p1:inst-build-system-event
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
