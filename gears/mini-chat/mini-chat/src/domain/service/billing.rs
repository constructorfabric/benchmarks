//! Shared billing helpers: billing outcome derivation (DESIGN §5.8) and the usage / audit
//! event builders used by stream finalization and orphan finalization.

use mini_chat_sdk::{
    AuditEvent, AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, TurnAuditEvent,
    TurnMutationAuditEvent, UsageEvent, UsageTokens,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::error::stream_codes;
use crate::domain::outbox_payloads::turn_dedupe_key;
use crate::domain::service::quota::SettlementMethod;

/// Billing outcome of a finalized turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingOutcome {
    Completed,
    Failed,
    Aborted,
}

impl BillingOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// Internal terminal state of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Completed,
    Failed,
    Cancelled,
}

impl TerminalState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// "Usage known" for failed turns: at least one non-zero token count.
#[must_use]
pub fn usage_known(usage: Option<&UsageTokens>) -> bool {
    usage.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

/// Normative billing outcome derivation (DESIGN §5.8 table).
#[must_use]
pub fn derive_billing(
    state: TerminalState,
    error_code: Option<&str>,
    usage: Option<&UsageTokens>,
) -> (BillingOutcome, SettlementMethod) {
    match state {
        TerminalState::Completed => (BillingOutcome::Completed, SettlementMethod::Actual),
        TerminalState::Cancelled => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        TerminalState::Failed => match error_code {
            Some(stream_codes::ORPHAN_TIMEOUT) => {
                (BillingOutcome::Aborted, SettlementMethod::Estimated)
            }
            Some(
                stream_codes::CONTEXT_LENGTH_EXCEEDED
                | stream_codes::TURN_SETUP_FAILED
                | "validation_error"
                | "input_too_long",
            ) => (BillingOutcome::Failed, SettlementMethod::Released),
            Some(
                stream_codes::PROVIDER_ERROR
                | stream_codes::PROVIDER_TIMEOUT
                | stream_codes::RATE_LIMITED
                | stream_codes::WEB_SEARCH_CALLS_EXCEEDED
                | stream_codes::CODE_INTERPRETER_CALLS_EXCEEDED
                | stream_codes::AGENTIC_ITERATIONS_EXCEEDED
                | stream_codes::UNEXPECTED_TOOL_USE
                | stream_codes::MESSAGE_PERSISTENCE_FAILED,
            ) => {
                if usage_known(usage) {
                    (BillingOutcome::Failed, SettlementMethod::Actual)
                } else {
                    (BillingOutcome::Failed, SettlementMethod::Estimated)
                }
            }
            other => {
                tracing::error!(error_code = ?other, "unknown turn error_code; settling as estimated");
                (BillingOutcome::Failed, SettlementMethod::Estimated)
            }
        },
    }
}

/// Inputs of a turn usage event.
#[derive(Debug, Clone)]
pub struct UsageEventInput {
    pub tenant_id: Uuid,
    pub user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub effective_model: String,
    pub selected_model: String,
    pub terminal_state: TerminalState,
    pub outcome: BillingOutcome,
    pub method: SettlementMethod,
    /// Provider usage; `None` on estimated / released settlements.
    pub usage: Option<UsageTokens>,
    pub actual_credits_micro: i64,
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
}

/// Builds the usage event of a user turn (`dedupe_key = {tenant}/{turn}/{request}`).
#[must_use]
pub fn build_usage_event(i: &UsageEventInput) -> UsageEvent {
    let usage = match i.method {
        // `null` when the provider reported no usage (also for actual settlements).
        SettlementMethod::Actual => i.usage,
        SettlementMethod::Released => Some(UsageTokens::default()),
        SettlementMethod::Estimated => None,
    };
    UsageEvent {
        tenant_id: i.tenant_id,
        user_id: i.user_id,
        chat_id: i.chat_id,
        turn_id: Some(i.turn_id),
        request_id: i.request_id,
        effective_model: i.effective_model.clone(),
        selected_model: i.selected_model.clone(),
        terminal_state: i.terminal_state.as_str().to_owned(),
        billing_outcome: i.outcome.as_str().to_owned(),
        usage,
        actual_credits_micro: i.actual_credits_micro,
        settlement_method: i.method.as_str().to_owned(),
        policy_version_applied: i.policy_version_applied,
        web_search_calls: i.web_search_calls,
        code_interpreter_calls: i.code_interpreter_calls,
        file_search_calls: i.file_search_calls,
        timestamp: OffsetDateTime::now_utc(),
        requester_type: "user".to_owned(),
        dedupe_key: turn_dedupe_key(i.tenant_id, i.turn_id, i.request_id),
        system_task_type: None,
    }
}

/// Inputs of a turn audit event.
#[derive(Debug, Clone)]
pub struct TurnAuditInput {
    pub tenant_id: Uuid,
    pub requester_user_id: Option<Uuid>,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal_state: TerminalState,
    pub error_code: Option<String>,
    pub usage: Option<UsageTokens>,
    pub latency_ms: u64,
    pub web_search_calls: u32,
    pub file_search_calls: u32,
    /// `allow`, `downgrade` or `unknown` (orphan watchdog).
    pub quota_decision: String,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
}

/// Builds the `turn_completed` / `turn_failed` audit event.
#[must_use]
pub fn build_turn_audit_event(i: &TurnAuditInput) -> AuditEvent {
    let event_type = if i.terminal_state == TerminalState::Completed {
        "turn_completed"
    } else {
        "turn_failed"
    };
    AuditEvent::Turn(TurnAuditEvent {
        event_type: event_type.to_owned(),
        tenant_id: i.tenant_id,
        requester_user_id: i.requester_user_id,
        requester_type: "user".to_owned(),
        chat_id: i.chat_id,
        turn_id: i.turn_id,
        request_id: i.request_id,
        selected_model: i.selected_model.clone(),
        effective_model: i.effective_model.clone(),
        terminal_state: i.terminal_state.as_str().to_owned(),
        error_code: i.error_code.clone(),
        usage: i.usage,
        latency_ms: i.latency_ms,
        tool_calls: AuditToolCalls {
            web_search_calls: i.web_search_calls,
            file_search_calls: i.file_search_calls,
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: i.quota_decision.clone(),
                downgrade_from: i.downgrade_from.clone(),
                downgrade_reason: i.downgrade_reason.clone(),
            },
            license: None,
            quota_scope: None,
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        trace_id: None,
        timestamp: OffsetDateTime::now_utc(),
    })
}

/// Builds a `turn_retry` / `turn_edit` / `turn_delete` audit event.
#[must_use]
pub fn build_mutation_audit_event(
    event_type: &str,
    tenant_id: Uuid,
    actor_user_id: Uuid,
    chat_id: Uuid,
    original_request_id: Uuid,
    new_request_id: Option<Uuid>,
) -> AuditEvent {
    let is_delete = event_type == "turn_delete";
    AuditEvent::TurnMutation(TurnMutationAuditEvent {
        event_type: event_type.to_owned(),
        tenant_id,
        actor_user_id,
        chat_id,
        original_request_id: (!is_delete).then_some(original_request_id),
        new_request_id,
        request_id: is_delete.then_some(original_request_id),
        timestamp: OffsetDateTime::now_utc(),
    })
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod billing_tests;
