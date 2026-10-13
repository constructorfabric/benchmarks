//! Shared billing outcome derivation and turn event builders (DESIGN
//! sections 5.7 "Turn Finalization Contract", 5.8 "Internal Error Code
//! Taxonomy" / "Normative Billing Outcome Derivation", 5.9, 3.9 "Audit Events
//! for Turn Mutations", ADR-0009).
//!
//! [`derive_billing`] is the single implementation of the authoritative
//! mapping table: every finalization path (stream completion, error,
//! disconnect, orphan watchdog) classifies its terminal condition through it
//! and never keys billing off `chat_turns.state` alone.
//!
//! | Terminal condition | `billing_outcome` | `settlement_method` |
//! |---|---|---|
//! | `completed` | `completed` | `actual` (zero or missing usage charges 0) |
//! | `failed` + post-provider code ([`POST_PROVIDER_ERROR_CODES`]) | `failed` | `actual` when usage is known, else `estimated` |
//! | `failed` + pre-provider code ([`PRE_PROVIDER_ERROR_CODES`]) | `failed` | `released` |
//! | `cancelled` | `aborted` | `estimated` |
//! | `failed` + `orphan_timeout` | `aborted` | `estimated` |
//! | `failed` + any other or no code | `failed` | `estimated`, flagged `unknown_error_code` |
//!
//! "Usage known" for a failed turn requires a usage object with a non-zero
//! input or output count (no-free-cancel rule).
//!
//! [`turn_usage_event`] and [`turn_audit_event`] build the SDK outbox payloads
//! of a user turn; the audit event never carries content (ADR-0009).

use mini_chat_sdk::{
    PolicyDecisions, QuotaDecision, ToolCalls, TurnAuditEvent, TurnAuditEventType, UsageEvent,
    UsageTokens, dedupe_key_for_turn,
};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::ports::TurnState;

// ── Error codes ──────────────────────────────────────────────────────────

/// `chat_turns.error_code` of the orphan watchdog.
pub const ORPHAN_TIMEOUT: &str = "orphan_timeout";

/// `chat_turns.error_code` of a turn setup failure before the provider call
/// (pre-provider: the reserve is released). Never sent over SSE.
pub const TURN_SETUP_FAILED: &str = "turn_setup_failed";

/// `chat_turns.error_code` of a retry / edit turn whose context does not fit
/// the model budget after the mutation committed (pre-provider). Never sent
/// over SSE.
pub const CONTEXT_LENGTH_EXCEEDED: &str = "context_length_exceeded";

/// `chat_turns.error_code` of a retry / edit turn whose reserve re-check
/// failed after the mutation committed. No reserve exists, so the code is
/// never settled (DESIGN section 5.8 "Internal Error Code Taxonomy"). Never
/// sent over SSE.
pub const QUOTA_EXCEEDED: &str = "quota_exceeded";

/// Codes produced after the provider call started: settled `actual` when the
/// provider reported usage, else `estimated`.
pub const POST_PROVIDER_ERROR_CODES: &[&str] = &[
    "provider_error",
    "provider_timeout",
    "rate_limited",
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];

/// Codes produced before the provider call: the reserve is released.
pub const PRE_PROVIDER_ERROR_CODES: &[&str] = &[
    CONTEXT_LENGTH_EXCEEDED,
    "validation_error",
    "input_too_long",
    TURN_SETUP_FAILED,
];

/// `requester_type` of a user turn.
pub const REQUESTER_USER: &str = "user";

// ── Value sets ───────────────────────────────────────────────────────────

/// Terminal internal state of a turn (`chat_turns.state` without `running`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalState {
    Completed,
    Failed,
    Cancelled,
}

impl TerminalState {
    /// Stored value, also the usage event `terminal_state`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The terminal state of `state`; `None` for `running`.
    #[must_use]
    pub const fn from_turn_state(state: TurnState) -> Option<Self> {
        match state {
            TurnState::Running => None,
            TurnState::Completed => Some(Self::Completed),
            TurnState::Failed => Some(Self::Failed),
            TurnState::Cancelled => Some(Self::Cancelled),
        }
    }

    /// The internal turn state.
    #[must_use]
    pub const fn turn_state(self) -> TurnState {
        match self {
            Self::Completed => TurnState::Completed,
            Self::Failed => TurnState::Failed,
            Self::Cancelled => TurnState::Cancelled,
        }
    }
}

/// Usage event `billing_outcome` of a turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BillingOutcome {
    Completed,
    Failed,
    Aborted,
}

impl BillingOutcome {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// Usage event `settlement_method` of a turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettlementMethod {
    /// Provider-reported usage.
    Actual,
    /// `credits_micro(estimated_input_tokens, minimal_generation_floor_applied)`.
    Estimated,
    /// Zero charge, the reserve is released (pre-provider failure).
    Released,
}

impl SettlementMethod {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Result of the billing outcome derivation.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BillingDecision {
    pub outcome: BillingOutcome,
    pub method: SettlementMethod,
    /// The error code is outside the taxonomy (classified `failed` /
    /// `estimated`).
    pub unknown_error_code: bool,
}

impl BillingDecision {
    const fn of(outcome: BillingOutcome, method: SettlementMethod) -> Self {
        Self {
            outcome,
            method,
            unknown_error_code: false,
        }
    }
}

// ── Derivation ───────────────────────────────────────────────────────────

/// Whether `usage` counts as reported usage of a failed turn: present with a
/// non-zero input or output count.
#[must_use]
pub fn usage_known(usage: Option<&UsageTokens>) -> bool {
    usage.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

/// The billing outcome and settlement method of a terminal condition (the
/// authoritative mapping table of section 5.8).
///
/// An error code outside the taxonomy (or a failed turn without a code) is
/// logged at `error` and classified `failed` / `estimated` with
/// [`BillingDecision::unknown_error_code`] set.
#[must_use]
pub fn derive_billing(
    terminal: TerminalState,
    error_code: Option<&str>,
    usage: Option<&UsageTokens>,
) -> BillingDecision {
    match terminal {
        TerminalState::Completed => {
            BillingDecision::of(BillingOutcome::Completed, SettlementMethod::Actual)
        }
        TerminalState::Cancelled => {
            BillingDecision::of(BillingOutcome::Aborted, SettlementMethod::Estimated)
        }
        TerminalState::Failed => derive_failed(error_code, usage),
    }
}

fn derive_failed(error_code: Option<&str>, usage: Option<&UsageTokens>) -> BillingDecision {
    match error_code {
        Some(ORPHAN_TIMEOUT) => {
            BillingDecision::of(BillingOutcome::Aborted, SettlementMethod::Estimated)
        }
        Some(code) if POST_PROVIDER_ERROR_CODES.contains(&code) => {
            let method = if usage_known(usage) {
                SettlementMethod::Actual
            } else {
                SettlementMethod::Estimated
            };
            BillingDecision::of(BillingOutcome::Failed, method)
        }
        Some(code) if PRE_PROVIDER_ERROR_CODES.contains(&code) => {
            BillingDecision::of(BillingOutcome::Failed, SettlementMethod::Released)
        }
        other => {
            tracing::error!(
                target: "mini_chat.billing",
                error_code = other.unwrap_or("<none>"),
                "unknown error code at settlement; billed as failed/estimated"
            );
            BillingDecision {
                outcome: BillingOutcome::Failed,
                method: SettlementMethod::Estimated,
                unknown_error_code: true,
            }
        }
    }
}

// ── Event builders ───────────────────────────────────────────────────────

/// Identities of a user turn shared by its usage and audit events.
#[domain_model]
#[allow(
    clippy::struct_field_names,
    reason = "field names are the usage / audit payload keys"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnIdentity {
    pub tenant_id: Uuid,
    /// Requesting user (`chat_turns.requester_user_id`).
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
}

impl TurnIdentity {
    /// The canonical usage dedupe key `{tenant}/{turn}/{request}` (simple
    /// UUID form).
    #[must_use]
    pub fn dedupe_key(&self) -> String {
        dedupe_key_for_turn(self.tenant_id, self.turn_id, self.request_id)
    }
}

/// Inputs of a user turn's usage event (sections 5.4.4, 5.7-5.9).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnUsageInput {
    pub identity: TurnIdentity,
    /// `chats.model` (the effective model on the orphan watchdog path).
    pub selected_model: String,
    /// `chat_turns.effective_model`.
    pub effective_model: String,
    pub terminal: TerminalState,
    pub billing: BillingDecision,
    /// Provider-reported usage, if any.
    pub usage: Option<UsageTokens>,
    /// Committed credits of the settlement (authoritative for CCM).
    pub actual_credits_micro: i64,
    pub policy_version_applied: u64,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
    pub file_search_calls: u32,
    pub timestamp: OffsetDateTime,
}

/// The usage outbox payload of a user turn.
///
/// `usage` follows the settlement method: the provider usage on `actual`
/// (`null` when the provider reported none), `null` on `estimated` (the
/// estimated split is not emitted), zero input/output counts on `released`.
#[must_use]
pub fn turn_usage_event(input: TurnUsageInput) -> UsageEvent {
    let usage = match input.billing.method {
        SettlementMethod::Actual => input.usage,
        SettlementMethod::Estimated => None,
        SettlementMethod::Released => Some(UsageTokens::default()),
    };
    UsageEvent {
        tenant_id: input.identity.tenant_id,
        user_id: Some(input.identity.user_id),
        chat_id: Some(input.identity.chat_id),
        turn_id: Some(input.identity.turn_id),
        request_id: input.identity.request_id,
        effective_model: input.effective_model,
        selected_model: input.selected_model,
        terminal_state: input.terminal.as_str().to_owned(),
        billing_outcome: input.billing.outcome.as_str().to_owned(),
        usage,
        actual_credits_micro: input.actual_credits_micro,
        settlement_method: input.billing.method.as_str().to_owned(),
        policy_version_applied: input.policy_version_applied,
        web_search_calls: input.web_search_calls,
        code_interpreter_calls: input.code_interpreter_calls,
        file_search_calls: input.file_search_calls,
        timestamp: input.timestamp,
        requester_type: REQUESTER_USER.to_owned(),
        dedupe_key: input.identity.dedupe_key(),
        system_task_type: None,
    }
}

/// The audit quota decision of a turn whose preflight decision is not known
/// (orphan finalization).
#[must_use]
pub fn unknown_quota_decision() -> QuotaDecision {
    QuotaDecision {
        decision: QuotaDecision::UNKNOWN.to_owned(),
        downgrade_from: None,
        downgrade_reason: None,
    }
}

/// Inputs of a user turn's finalization audit event (section 3.9).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAuditInput {
    pub identity: TurnIdentity,
    pub selected_model: String,
    pub effective_model: String,
    pub terminal: TerminalState,
    /// `chat_turns.error_code` of a failed turn.
    pub error_code: Option<String>,
    pub usage: Option<UsageTokens>,
    pub latency_ms: u64,
    pub web_search_calls: u32,
    pub file_search_calls: u32,
    pub quota_decision: QuotaDecision,
    pub trace_id: Option<String>,
    pub timestamp: OffsetDateTime,
}

/// The turn finalization audit event: `turn_completed` for a completed turn,
/// `turn_failed` for every other terminal state; content fields are empty.
#[must_use]
pub fn turn_audit_event(input: TurnAuditInput) -> TurnAuditEvent {
    let event_type = match input.terminal {
        TerminalState::Completed => TurnAuditEventType::Completed,
        TerminalState::Failed | TerminalState::Cancelled => TurnAuditEventType::Failed,
    };
    TurnAuditEvent {
        event_type,
        timestamp: input.timestamp,
        tenant_id: input.identity.tenant_id,
        requester_type: REQUESTER_USER.to_owned(),
        user_id: input.identity.user_id,
        chat_id: input.identity.chat_id,
        turn_id: input.identity.turn_id,
        request_id: input.identity.request_id,
        selected_model: input.selected_model,
        effective_model: input.effective_model,
        usage: input.usage,
        latency_ms: input.latency_ms,
        tool_calls: ToolCalls {
            web_search_calls: input.web_search_calls,
            file_search_calls: input.file_search_calls,
        },
        policy_decisions: PolicyDecisions {
            quota: input.quota_decision,
        },
        error_code: input.error_code,
        trace_id: input.trace_id,
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        license: None,
        quota_scope: None,
    }
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod tests;
