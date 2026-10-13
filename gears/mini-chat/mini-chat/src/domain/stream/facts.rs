//! The facts of a running turn and the usage and audit events built from them.

use mini_chat_sdk::{
    BillingOutcome, PolicyDecisions, QuotaDecision, RequesterType, SettlementMethod, TokenUsage,
    ToolCalls, TurnAuditEvent, TurnAuditEventType, TurnLatency, UsageEvent, UsageTerminalState,
};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::model::ToolCounts;
use crate::domain::billing::{Settlement, SettlementBasis};
use crate::domain::summary::SummaryTrigger;

/// What the provider task knows about its turn.
#[domain_model]
#[derive(Debug, Clone)]
pub struct TurnFacts {
    /// The turn.
    pub turn_id: Uuid,
    /// The owning tenant.
    pub tenant_id: Uuid,
    /// The requesting user.
    pub user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The turn's request id.
    pub request_id: Uuid,
    /// The pre-allocated assistant message id.
    pub assistant_message_id: Uuid,
    /// The chat's model.
    pub selected_model: String,
    /// The model resolved at preflight.
    pub effective_model: String,
    /// `allow` or `downgrade`.
    pub quota_decision: &'static str,
    /// The selected model, when downgraded.
    pub downgrade_from: Option<String>,
    /// Why, when downgraded.
    pub downgrade_reason: Option<&'static str>,
    /// The policy version of the preflight.
    pub policy_version: i64,
    /// When the turn started (selects the quota period rows).
    pub started_at: OffsetDateTime,
    /// The persisted preflight facts and the multipliers of the effective model.
    pub basis: SettlementBasis,
    /// The turn ran on the premium tier.
    pub premium: bool,
    /// The token estimate of the thread summary in the context.
    pub thread_summary_tokens: Option<i64>,
    /// OpenTelemetry trace id of the request that started the turn, when a span was active.
    pub trace_id: Option<String>,
    /// The summary trigger the turn's context assembly evaluated; `None` when neither condition
    /// held or the worker is disabled.
    pub summary_trigger: Option<SummaryTrigger>,
}

/// The terminal facts an event pair is built from.
#[domain_model]
#[derive(Debug, Clone)]
pub struct TerminalFacts<'a> {
    /// The terminal state.
    pub state: UsageTerminalState,
    /// The stored error code of a failed turn.
    pub error_code: Option<&'a str>,
    /// Provider usage, when reported.
    pub usage: Option<TokenUsage>,
    /// The billing outcome.
    pub billing_outcome: BillingOutcome,
    /// Completed tool calls.
    pub tool_counts: ToolCounts,
    /// Latency figures.
    pub latency: TurnLatency,
    /// The finalization instant.
    pub now: OffsetDateTime,
}

/// `{tenant}/{turn}/{request}` in simple form (DESIGN section 5.6).
#[must_use]
pub fn dedupe_key(tenant_id: Uuid, turn_id: Uuid, request_id: Uuid) -> String {
    format!(
        "{}/{}/{}",
        tenant_id.simple(),
        turn_id.simple(),
        request_id.simple()
    )
}

/// Builds the usage event and the turn audit event of a finalization.
#[must_use]
pub fn build_events(
    facts: &TurnFacts,
    terminal: &TerminalFacts<'_>,
    settlement: &Settlement,
) -> (UsageEvent, TurnAuditEvent) {
    let reported_usage = (settlement.method == SettlementMethod::Actual)
        .then_some(terminal.usage)
        .flatten();
    let usage_event = UsageEvent {
        tenant_id: facts.tenant_id,
        user_id: Some(facts.user_id),
        chat_id: facts.chat_id,
        turn_id: Some(facts.turn_id),
        request_id: facts.request_id,
        effective_model: facts.effective_model.clone(),
        selected_model: facts.selected_model.clone(),
        terminal_state: terminal.state,
        billing_outcome: terminal.billing_outcome,
        usage: reported_usage,
        actual_credits_micro: settlement.committed_credits_micro,
        settlement_method: settlement.method,
        policy_version_applied: facts.policy_version,
        web_search_calls: terminal.tool_counts.web_search,
        code_interpreter_calls: terminal.tool_counts.code_interpreter,
        file_search_calls: terminal.tool_counts.file_search,
        timestamp: terminal.now,
        requester_type: RequesterType::User,
        dedupe_key: dedupe_key(facts.tenant_id, facts.turn_id, facts.request_id),
        system_task_type: None,
    };
    let audit_event = TurnAuditEvent {
        event_type: if terminal.state == UsageTerminalState::Completed {
            TurnAuditEventType::Completed
        } else {
            TurnAuditEventType::Failed
        },
        timestamp: terminal.now,
        tenant_id: facts.tenant_id,
        requester_type: RequesterType::User,
        user_id: facts.user_id,
        chat_id: facts.chat_id,
        turn_id: facts.turn_id,
        request_id: facts.request_id,
        selected_model: facts.selected_model.clone(),
        effective_model: facts.effective_model.clone(),
        terminal_state: terminal.state,
        error_code: terminal.error_code.map(str::to_owned),
        usage: terminal.usage,
        latency: terminal.latency,
        tool_calls: ToolCalls {
            web_search_calls: terminal.tool_counts.web_search,
            file_search_calls: terminal.tool_counts.file_search,
        },
        policy_decisions: PolicyDecisions {
            quota: QuotaDecision {
                decision: facts.quota_decision.to_owned(),
                downgrade_from: facts.downgrade_from.clone(),
                downgrade_reason: facts.downgrade_reason.map(str::to_owned),
            },
        },
        prompt: String::new(),
        response: String::new(),
        trace_id: facts.trace_id.clone(),
    };
    (usage_event, audit_event)
}

#[cfg(test)]
#[path = "facts_tests.rs"]
mod tests;
