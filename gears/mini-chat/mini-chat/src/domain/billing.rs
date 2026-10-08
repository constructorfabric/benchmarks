//! Billing outcome derivation (DESIGN section 5.8, "Normative Billing Outcome
//! Derivation").
//!
//! The usage event's `billing_outcome` and `settlement_method` are derived here,
//! once, from the terminal condition of a turn. They are never read off
//! `chat_turns.state` directly: the orphan watchdog stores `failed` but bills as
//! `aborted`.

use crate::domain::enums::TurnState;

/// Terminal turn states (`chat_turns.state` once the turn left `running`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnTerminalState {
    Completed,
    Failed,
    Cancelled,
}

impl TryFrom<TurnState> for TurnTerminalState {
    /// The turn is still `running`.
    type Error = TurnState;

    fn try_from(state: TurnState) -> Result<Self, Self::Error> {
        match state {
            TurnState::Completed => Ok(Self::Completed),
            TurnState::Failed => Ok(Self::Failed),
            TurnState::Cancelled => Ok(Self::Cancelled),
            TurnState::Running => Err(state),
        }
    }
}

/// Outbox `billing_outcome`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillingOutcome {
    Completed,
    Failed,
    Aborted,
}

impl BillingOutcome {
    /// The value carried by the usage event.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// Outbox `settlement_method`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettlementMethod {
    /// Settled from provider-reported usage.
    Actual,
    /// Settled with the deterministic estimation formula.
    Estimated,
    /// The reserve is released; nothing is charged.
    Released,
}

impl SettlementMethod {
    /// The value carried by the usage event.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Failures after the provider request started: settled by actual usage when the
/// provider reported any, else estimated. Never released.
const POST_PROVIDER_CODES: &[&str] = &[
    "provider_error",
    "provider_timeout",
    "rate_limited",
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];

/// Failures before the provider request was issued: zero charge.
const PRE_PROVIDER_CODES: &[&str] = &[
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// Whether `code` is one of the error codes classified by [`derive_billing`]
/// (DESIGN section 5.8, "Internal Error Code Taxonomy"). Settlement code logs
/// an unknown code at error level (critical) before billing it as
/// `Failed` / `Estimated`.
#[must_use]
pub fn is_known_error_code(code: &str) -> bool {
    code == "orphan_timeout"
        || POST_PROVIDER_CODES.contains(&code)
        || PRE_PROVIDER_CODES.contains(&code)
}

/// Derive the billing outcome and settlement method of a terminal turn.
///
/// `usage_known` is whether the provider reported usage. An unknown or missing
/// error code on a failed turn is `Failed` / `Estimated` (the caller logs it).
/// `quota_exceeded` (an unstarted retry/edit turn) is deliberately not listed:
/// no reserve exists, so no settlement or outbox event is produced for it.
#[must_use]
pub fn derive_billing(
    state: TurnTerminalState,
    error_code: Option<&str>,
    usage_known: bool,
) -> (BillingOutcome, SettlementMethod) {
    match state {
        TurnTerminalState::Completed => (BillingOutcome::Completed, SettlementMethod::Actual),
        TurnTerminalState::Cancelled => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        TurnTerminalState::Failed => match error_code {
            Some("orphan_timeout") => (BillingOutcome::Aborted, SettlementMethod::Estimated),
            Some(code) if POST_PROVIDER_CODES.contains(&code) => (
                BillingOutcome::Failed,
                if usage_known {
                    SettlementMethod::Actual
                } else {
                    SettlementMethod::Estimated
                },
            ),
            Some(code) if PRE_PROVIDER_CODES.contains(&code) => {
                (BillingOutcome::Failed, SettlementMethod::Released)
            }
            _ => (BillingOutcome::Failed, SettlementMethod::Estimated),
        },
    }
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod billing_tests;
