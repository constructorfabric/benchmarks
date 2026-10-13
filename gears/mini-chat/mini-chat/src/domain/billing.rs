//! Billing outcome derivation and settlement arithmetic (DESIGN §5.7–§5.9).

use mini_chat_sdk::UsageTokens;

use crate::domain::credits::{CreditsError, credits_micro_checked};

/// Terminal trigger of a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// Provider `response.completed` / `response.incomplete`.
    Completed,
    /// `failed` with this error code.
    Failed(String),
    /// Client disconnect.
    Cancelled,
    /// Orphan watchdog.
    Orphan,
}

/// Outbox `billing_outcome` / `settlement_method` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingClass {
    pub billing_outcome: &'static str,
    /// `actual` | `estimated` | `released`
    pub method: &'static str,
}

const PRE_PROVIDER: &[&str] = &[
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// Whether usage counts as "known" for a failed turn (at least one non-zero field).
#[must_use]
pub fn usage_known(u: Option<&UsageTokens>) -> bool {
    u.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

/// Normative billing outcome derivation (§5.8 table).
#[must_use]
pub fn classify(terminal: &Terminal, usage: Option<&UsageTokens>) -> BillingClass {
    match terminal {
        Terminal::Completed => BillingClass {
            billing_outcome: "completed",
            method: "actual",
        },
        Terminal::Cancelled | Terminal::Orphan => BillingClass {
            billing_outcome: "aborted",
            method: "estimated",
        },
        Terminal::Failed(code) if PRE_PROVIDER.contains(&code.as_str()) => BillingClass {
            billing_outcome: "failed",
            method: "released",
        },
        Terminal::Failed(code) => {
            let known = matches!(
                code.as_str(),
                "provider_error"
                    | "provider_timeout"
                    | "rate_limited"
                    | "web_search_calls_exceeded"
                    | "code_interpreter_calls_exceeded"
                    | "agentic_iterations_exceeded"
                    | "unexpected_tool_use"
                    | "message_persistence_failed"
            );
            if !known {
                tracing::error!(code = %code, "unknown_error_code in settlement");
            }
            BillingClass {
                billing_outcome: "failed",
                method: if known && usage_known(usage) {
                    "actual"
                } else {
                    "estimated"
                },
            }
        }
    }
}

/// Persisted preflight reserve of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_field_names)] // reason: field names mirror the persisted turn columns
pub struct Reserve {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
}

/// Result of a settlement computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // reason: independent settlement flags, not a state machine
pub struct Settlement {
    pub committed_credits_micro: i64,
    /// Actual tokens added to `quota_usage` telemetry (0 unless actual).
    pub telemetry_input_tokens: i64,
    pub telemetry_output_tokens: i64,
    /// Whether tool call counters are added (actual and estimated).
    pub count_tool_calls: bool,
    pub overshoot: bool,
    pub overshoot_capped: bool,
}

/// Compute the settlement for `method`.
///
/// # Errors
/// Credit computation failures (bounds, overflow).
pub fn settle(
    method: &str,
    reserve: Reserve,
    usage: Option<&UsageTokens>,
    in_mult: i64,
    out_mult: i64,
    overshoot_tolerance: f64,
) -> Result<Settlement, CreditsError> {
    match method {
        "actual" => {
            let u = usage.copied().unwrap_or_default();
            let actual_tokens = u.input_tokens + u.output_tokens;
            let actual_credits =
                credits_micro_checked(u.input_tokens, u.output_tokens, in_mult, out_mult)?;
            let mut overshoot = false;
            let mut capped = false;
            let committed = if actual_tokens > reserve.reserve_tokens && reserve.reserve_tokens > 0 {
                overshoot = true;
                #[allow(clippy::cast_precision_loss)]
                let factor = actual_tokens as f64 / reserve.reserve_tokens as f64;
                if factor <= overshoot_tolerance {
                    actual_credits
                } else {
                    capped = true;
                    reserve.reserved_credits_micro
                }
            } else {
                actual_credits
            };
            Ok(Settlement {
                committed_credits_micro: committed,
                telemetry_input_tokens: u.input_tokens,
                telemetry_output_tokens: u.output_tokens,
                count_tool_calls: true,
                overshoot,
                overshoot_capped: capped,
            })
        }
        "estimated" => {
            let est_input = (reserve.reserve_tokens - reserve.max_output_tokens_applied).max(0);
            let committed = credits_micro_checked(
                est_input,
                reserve.minimal_generation_floor_applied.max(0),
                in_mult,
                out_mult,
            )?;
            Ok(Settlement {
                committed_credits_micro: committed,
                telemetry_input_tokens: 0,
                telemetry_output_tokens: 0,
                count_tool_calls: true,
                overshoot: false,
                overshoot_capped: false,
            })
        }
        _ => Ok(Settlement {
            committed_credits_micro: 0,
            telemetry_input_tokens: 0,
            telemetry_output_tokens: 0,
            count_tool_calls: false,
            overshoot: false,
            overshoot_capped: false,
        }),
    }
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod tests;
