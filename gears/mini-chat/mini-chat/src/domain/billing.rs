//! Billing outcome derivation (D§5.8 "Normative Billing Outcome
//! Derivation") and settlement amounts (D§5.4.4, D§5.4.5, D§5.8).

use mini_chat_sdk::{TerminalState, UsageTokens};

use crate::domain::estimation::{CreditError, credits_micro};

pub use mini_chat_sdk::{BillingOutcome, SettlementMethod};

/// Persisted per-turn reserve fields (`chat_turns`), the only turn inputs
/// of a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnReserve {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
}

/// Amount a settlement commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    /// Credits added to `spent_credits_micro` and emitted as the usage
    /// event's `actual_credits_micro`.
    pub committed_credits_micro: i64,
    /// Provider-reported `(input, output)` tokens for the `total` bucket
    /// telemetry (actual settlements with usage only).
    pub actual_tokens_for_telemetry: Option<(i64, i64)>,
    /// Actual settlement with `input + output > reserve_tokens`.
    pub overshoot: bool,
}

/// Billing outcome and settlement method of a terminal turn.
#[must_use]
pub fn derive(
    state: TerminalState,
    error_code: Option<&str>,
    usage: Option<&UsageTokens>,
) -> (BillingOutcome, SettlementMethod) {
    match state {
        TerminalState::Completed => (BillingOutcome::Completed, SettlementMethod::Actual),
        TerminalState::Cancelled => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        TerminalState::Failed => derive_failed(error_code, usage),
    }
}

/// Error codes of failures after the provider call started.
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

/// Error codes of failures before the provider call.
const PRE_PROVIDER_CODES: &[&str] = &[
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

fn derive_failed(
    error_code: Option<&str>,
    usage: Option<&UsageTokens>,
) -> (BillingOutcome, SettlementMethod) {
    match error_code {
        Some("orphan_timeout") => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        Some(code) if POST_PROVIDER_CODES.contains(&code) => {
            // "Usage known" on a failed turn needs a non-zero field.
            let known = usage.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0);
            let method = if known {
                SettlementMethod::Actual
            } else {
                SettlementMethod::Estimated
            };
            (BillingOutcome::Failed, method)
        }
        Some(code) if PRE_PROVIDER_CODES.contains(&code) => {
            (BillingOutcome::Failed, SettlementMethod::Released)
        }
        other => {
            tracing::error!(
                error_code = other.unwrap_or("<none>"),
                "unknown_error_code: failed turn settled as estimated"
            );
            (BillingOutcome::Failed, SettlementMethod::Estimated)
        }
    }
}

/// Credits committed by a settlement of `method` (`mults` = the effective
/// model's `(in, out)` multipliers of the turn's policy version).
///
/// # Errors
/// `CreditError` when the credit computation fails.
pub fn settle_amount(
    method: SettlementMethod,
    turn: &TurnReserve,
    usage: Option<&UsageTokens>,
    tolerance: f64,
    mults: (i64, i64),
) -> Result<Settlement, CreditError> {
    let (in_mult, out_mult) = mults;
    match method {
        SettlementMethod::Actual => {
            let u = usage.copied().unwrap_or_default();
            let actual = credits_micro(u.input_tokens, u.output_tokens, in_mult, out_mult)?;
            let actual_tokens = u.input_tokens + u.output_tokens;
            let overshoot = actual_tokens > turn.reserve_tokens;
            // Float ratio (D§5.4.5); a zero reserve gives +inf and is capped.
            #[allow(clippy::cast_precision_loss)]
            let capped =
                overshoot && (actual_tokens as f64) / (turn.reserve_tokens as f64) > tolerance;
            Ok(Settlement {
                committed_credits_micro: if capped {
                    turn.reserved_credits_micro
                } else {
                    actual
                },
                actual_tokens_for_telemetry: usage.map(|u| (u.input_tokens, u.output_tokens)),
                overshoot,
            })
        }
        SettlementMethod::Estimated => {
            // charged_tokens = min(reserve, est_input + floor): the floor is
            // capped at max_output_tokens_applied.
            let estimated_input = turn.reserve_tokens - turn.max_output_tokens_applied;
            let charged_output = turn
                .minimal_generation_floor_applied
                .min(turn.max_output_tokens_applied);
            Ok(Settlement {
                committed_credits_micro: credits_micro(
                    estimated_input,
                    charged_output,
                    in_mult,
                    out_mult,
                )?,
                actual_tokens_for_telemetry: None,
                overshoot: false,
            })
        }
        SettlementMethod::Released | SettlementMethod::None => Ok(Settlement {
            committed_credits_micro: 0,
            actual_tokens_for_telemetry: None,
            overshoot: false,
        }),
    }
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod tests;
