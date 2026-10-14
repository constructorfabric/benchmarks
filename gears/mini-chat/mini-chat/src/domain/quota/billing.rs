//! Billing outcome derivation (`cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation`):
//! the single shared mapping of DESIGN 5.8 from the terminal condition to the billing outcome
//! and the settlement method, used by the stream finalization and the orphan finalization.

use mini_chat_sdk::UsageTokens;
use toolkit_macros::domain_model;

/// The internal terminal state of `chat_turns.state` a finalizer moves a turn to.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Completed,
    Failed,
    Cancelled,
}

impl TerminalState {
    /// The `chat_turns.state` value and the usage event `terminal_state`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The billing outcome of a turn (the thread-summary `system_task` is not a turn outcome).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingOutcome {
    Completed,
    Failed,
    Aborted,
}

impl BillingOutcome {
    /// The usage event `billing_outcome` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// How the committed credits are computed.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementMethod {
    /// From the provider-reported usage.
    Actual,
    /// From the persisted reserve columns and the persisted floor.
    Estimated,
    /// Zero charge: the whole reserve is released.
    Released,
}

impl SettlementMethod {
    /// The usage event `settlement_method` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// The result of the derivation.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingDerivation {
    pub outcome: BillingOutcome,
    pub method: SettlementMethod,
    /// The `error_code` was not in the taxonomy: classified `failed` with `estimated`, logged
    /// as a critical error with the code.
    pub unknown_error_code: bool,
}

/// Derive the outcome from the terminal state and the `error_code`, never from the state
/// string alone. Usage of a failed turn is known only when `input_tokens` or `output_tokens`
/// is non-zero; a completed turn is always `actual`; a cancelled or orphan-timeout turn is
/// always `estimated` whatever usage is passed. `None` for `quota_exceeded`: no reserve exists,
/// so there is no billing outcome, no settlement and no usage event.
#[must_use]
pub fn derive_billing(
    state: TerminalState,
    error_code: Option<&str>,
    usage: Option<&UsageTokens>,
) -> Option<BillingDerivation> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-not-from-state
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-match
    // the outcome follows the trigger classification and the error code, never the state alone
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-usage-known
    // usage of a failed turn is known only with a non-zero token field: an object with both
    // fields 0, or none, takes the estimated path (a completed turn is not subject to this)
    let usage_known = usage.is_some_and(|u| u.input_tokens != 0 || u.output_tokens != 0);
    let by_usage = if usage_known {
        SettlementMethod::Actual
    } else {
        SettlementMethod::Estimated
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-usage-known
    match (state, error_code) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-completed
        (TerminalState::Completed, _) => Some(derivation(
            BillingOutcome::Completed,
            SettlementMethod::Actual,
        )),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-completed
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-cancelled
        (TerminalState::Cancelled, _) => Some(derivation(
            BillingOutcome::Aborted,
            SettlementMethod::Estimated,
        )),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-cancelled
        (TerminalState::Failed, code) => derive_failed(code, by_usage),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-match
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-not-from-state
}

/// A derivation of a code inside the taxonomy.
const fn derivation(outcome: BillingOutcome, method: SettlementMethod) -> BillingDerivation {
    BillingDerivation {
        outcome,
        method,
        unknown_error_code: false,
    }
}

/// The failed-state rows of the table: the outcome follows the `error_code`.
fn derive_failed(
    error_code: Option<&str>,
    by_usage: SettlementMethod,
) -> Option<BillingDerivation> {
    match error_code {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-provider-errors
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-tool-errors
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-no-free-cancel
        // the provider was already called: `actual` with known usage, otherwise `estimated`,
        // never `released`
        Some(
            "provider_error"
            | "provider_timeout"
            | "rate_limited"
            | "web_search_calls_exceeded"
            | "code_interpreter_calls_exceeded"
            | "agentic_iterations_exceeded"
            | "unexpected_tool_use"
            | "message_persistence_failed",
        ) => Some(derivation(BillingOutcome::Failed, by_usage)),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-no-free-cancel
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-tool-errors
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-provider-errors
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-pre-provider
        Some(
            "context_length_exceeded" | "validation_error" | "input_too_long" | "turn_setup_failed",
        ) => Some(derivation(
            BillingOutcome::Failed,
            SettlementMethod::Released,
        )),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-pre-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-orphan
        Some("orphan_timeout") => Some(derivation(
            BillingOutcome::Aborted,
            SettlementMethod::Estimated,
        )),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-orphan
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-quota-exceeded
        // no reserve exists: no billing outcome, no settlement, no usage event
        Some("quota_exceeded") => None,
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-quota-exceeded
        other => {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-unknown-code
            tracing::error!(
                error_code = other.unwrap_or("<none>"),
                "critical: unknown turn error code; settling as failed on the estimated path"
            );
            Some(BillingDerivation {
                outcome: BillingOutcome::Failed,
                method: SettlementMethod::Estimated,
                unknown_error_code: true,
            })
            // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-billing-outcome-derivation:p1:inst-outcome-unknown-code
        }
    }
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod billing_tests;
