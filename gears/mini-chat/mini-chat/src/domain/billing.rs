//! Billing outcome derivation and settlement arithmetic (DESIGN sections 5.4.4, 5.4.5, 5.7, 5.8).

use mini_chat_sdk::{BillingOutcome, SettlementMethod, TokenUsage, UsageTerminalState};
use toolkit_macros::domain_model;
use tracing::warn;

use super::credits::{CreditsError, credits_micro};

/// Error codes of failures after the provider call started.
const POST_PROVIDER_CODES: [&str; 8] = [
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
const PRE_PROVIDER_CODES: [&str; 4] = [
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// "Usage known": a usage object with at least one non-zero token count.
#[must_use]
pub fn usage_known(usage: Option<&TokenUsage>) -> bool {
    usage.is_some_and(|usage| usage.input_tokens > 0 || usage.output_tokens > 0)
}

/// The shared billing outcome derivation (DESIGN "Normative Billing Outcome Derivation").
///
/// `usage` is the provider usage of the terminal event, when there was one.
#[must_use]
pub fn derive_billing(
    terminal: UsageTerminalState,
    error_code: Option<&str>,
    usage: Option<&TokenUsage>,
) -> (BillingOutcome, SettlementMethod) {
    match terminal {
        UsageTerminalState::Completed => (BillingOutcome::Completed, SettlementMethod::Actual),
        UsageTerminalState::Cancelled => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        UsageTerminalState::Failed => match error_code {
            Some("orphan_timeout") => (BillingOutcome::Aborted, SettlementMethod::Estimated),
            Some(code) if PRE_PROVIDER_CODES.contains(&code) => {
                (BillingOutcome::Failed, SettlementMethod::Released)
            }
            Some(code) if POST_PROVIDER_CODES.contains(&code) => (
                BillingOutcome::Failed,
                if usage_known(usage) {
                    SettlementMethod::Actual
                } else {
                    SettlementMethod::Estimated
                },
            ),
            other => {
                warn!(error_code = ?other, "unknown turn error code classified as a failed, estimated settlement");
                (BillingOutcome::Failed, SettlementMethod::Estimated)
            }
        },
    }
}

/// The persisted preflight facts and the multipliers a settlement needs.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementBasis {
    /// `chat_turns.reserve_tokens`.
    pub reserve_tokens: i64,
    /// `chat_turns.max_output_tokens_applied`.
    pub max_output_tokens_applied: i64,
    /// `chat_turns.reserved_credits_micro`.
    pub reserved_credits_micro: i64,
    /// `chat_turns.minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: i64,
    /// Input multiplier of the effective model under the applied policy version.
    pub in_mult: i64,
    /// Output multiplier of the effective model under the applied policy version.
    pub out_mult: i64,
}

/// The computed settlement of a turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    /// How the credits were derived.
    pub method: SettlementMethod,
    /// The credits charged to quota (`committed_credits_micro`).
    pub committed_credits_micro: i64,
    /// Input tokens added to the telemetry counters (actual settlements only).
    pub telemetry_input_tokens: i64,
    /// Output tokens added to the telemetry counters (actual settlements only).
    pub telemetry_output_tokens: i64,
    /// An actual settlement whose tokens exceeded the reserve (counted by a metric).
    pub overshoot: bool,
}

/// Settles a turn.
///
/// `Actual` charges the provider usage with the overshoot rule (a turn whose tokens exceed the
/// reserve by more than `tolerance` is capped at the reserve), `Estimated` charges
/// `credits_micro(reserve - max_output, floor)` and `Released` charges nothing.
///
/// # Errors
/// [`CreditsError`] when the credit computation is refused.
pub fn settle(
    basis: &SettlementBasis,
    method: SettlementMethod,
    usage: Option<&TokenUsage>,
    tolerance: f64,
) -> Result<Settlement, CreditsError> {
    match method {
        SettlementMethod::Actual => {
            let usage = usage.copied().unwrap_or_default();
            let actual = credits_micro(
                usage.input_tokens,
                usage.output_tokens,
                basis.in_mult,
                basis.out_mult,
            )?;
            let actual_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
            let overshoot = actual_tokens > basis.reserve_tokens;
            let capped =
                overshoot && exceeds_tolerance(actual_tokens, basis.reserve_tokens, tolerance);
            Ok(Settlement {
                method,
                committed_credits_micro: if capped {
                    basis.reserved_credits_micro
                } else {
                    actual
                },
                telemetry_input_tokens: usage.input_tokens,
                telemetry_output_tokens: usage.output_tokens,
                overshoot,
            })
        }
        SettlementMethod::Estimated => {
            let estimated_input = basis
                .reserve_tokens
                .saturating_sub(basis.max_output_tokens_applied);
            Ok(Settlement {
                method,
                committed_credits_micro: credits_micro(
                    estimated_input,
                    basis.minimal_generation_floor_applied,
                    basis.in_mult,
                    basis.out_mult,
                )?,
                telemetry_input_tokens: 0,
                telemetry_output_tokens: 0,
                overshoot: false,
            })
        }
        SettlementMethod::Released | SettlementMethod::None => Ok(Settlement {
            method,
            committed_credits_micro: 0,
            telemetry_input_tokens: 0,
            telemetry_output_tokens: 0,
            overshoot: false,
        }),
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "token counts are far below 2^52; the ratio is compared in floating point as DESIGN requires"
)]
fn exceeds_tolerance(actual_tokens: i64, reserve_tokens: i64, tolerance: f64) -> bool {
    if reserve_tokens <= 0 {
        return true;
    }
    (actual_tokens as f64) / (reserve_tokens as f64) > tolerance
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod tests;
