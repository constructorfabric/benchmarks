//! Credit arithmetic and token estimation (DESIGN §5.3, §5.4.1, §5.5.4).

use mini_chat_sdk::EstimationBudgets;

/// Upper bound of a token count accepted by the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Upper bound of a credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULT: i64 = 10_000_000_000;

/// Failure of the checked credit computation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("invalid token count {0}")]
    InvalidTokenCount(i64),
    #[error("zero credit multiplier")]
    ZeroMultiplier,
    #[error("invalid credit multiplier {0}")]
    InvalidMultiplier(i64),
    #[error("credit computation overflow")]
    Overflow,
}

// Truncating division is intended: the remainder check rounds the result up.
#[allow(clippy::integer_division)]
fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

/// Canonical `credits_micro` with per-component `ceil_div` rounding and overflow checks.
///
/// # Errors
/// Out-of-range token counts or multipliers, or arithmetic overflow.
pub fn credits_micro(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditError> {
    for t in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKENS).contains(&t) {
            return Err(CreditError::InvalidTokenCount(t));
        }
    }
    for m in [in_mult, out_mult] {
        if m == 0 {
            return Err(CreditError::ZeroMultiplier);
        }
        if !(1..=MAX_MULT).contains(&m) {
            return Err(CreditError::InvalidMultiplier(m));
        }
    }
    let a = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditError::Overflow)?;
    let b = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditError::Overflow)?;
    ceil_div(a, 1_000_000)
        .checked_add(ceil_div(b, 1_000_000))
        .ok_or(CreditError::Overflow)
}

/// Estimated tokens of a text, from the model's estimation budgets:
/// `ceil((ceil(bytes / bptc) + fixed_overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(text: &str, b: &EstimationBudgets) -> i64 {
    #[allow(clippy::integer_division)] // exact constant: overflow-safe saturation cap
    let bytes = i64::try_from(text.len()).unwrap_or(i64::MAX / 4);
    let bptc = i64::from(b.bytes_per_token_conservative.max(1));
    let base = ceil_div(bytes, bptc) + i64::from(b.fixed_overhead_tokens);
    ceil_div(
        base.saturating_mul(100 + i64::from(b.safety_margin_pct)),
        100,
    )
}

/// Inputs of the preflight reserve of one candidate model.
// Each flag selects an independent per-tool surcharge of the canonical formula.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ReserveInputs {
    pub message_tokens: i64,
    pub prior_context_tokens: i64,
    pub image_count: u32,
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Estimated input tokens (DESIGN §5.4.1 canonical formula).
#[must_use]
pub fn estimated_input_tokens(inputs: &ReserveInputs, b: &EstimationBudgets) -> i64 {
    let mut total = inputs.message_tokens + inputs.prior_context_tokens;
    total += i64::from(inputs.image_count) * i64::from(b.image_token_budget);
    if inputs.file_search {
        total += i64::from(b.tool_surcharge_tokens);
    }
    if inputs.web_search {
        total += i64::from(b.web_search_surcharge_tokens);
    }
    if inputs.code_interpreter {
        total += i64::from(b.code_interpreter_surcharge_tokens);
    }
    total
}

/// Settlement method of a finalized turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementMethod {
    Actual,
    Estimated,
    Released,
}

impl SettlementMethod {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Billing outcome of a finalized turn (DESIGN §5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingOutcome {
    Completed,
    Failed,
    Aborted,
}

impl BillingOutcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

/// Error codes settled with `released` (pre-provider failures).
pub const RELEASED_CODES: &[&str] = &[
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// Normative billing outcome derivation (DESIGN §5.8): maps the terminal state,
/// the error code and whether non-zero usage is known to the outcome and method.
#[must_use]
pub fn derive_billing(
    state: &str,
    error_code: Option<&str>,
    usage_known: bool,
) -> (BillingOutcome, SettlementMethod) {
    match state {
        "completed" => (BillingOutcome::Completed, SettlementMethod::Actual),
        "cancelled" => (BillingOutcome::Aborted, SettlementMethod::Estimated),
        _ => match error_code {
            Some("orphan_timeout") => (BillingOutcome::Aborted, SettlementMethod::Estimated),
            Some(code) if RELEASED_CODES.contains(&code) => {
                (BillingOutcome::Failed, SettlementMethod::Released)
            }
            _ => {
                if usage_known {
                    (BillingOutcome::Failed, SettlementMethod::Actual)
                } else {
                    (BillingOutcome::Failed, SettlementMethod::Estimated)
                }
            }
        },
    }
}

/// Persisted reserve of a turn (immutable preflight columns).
#[derive(Debug, Clone, Copy)]
pub struct TurnReserve {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
}

/// Result of settling a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    pub method: SettlementMethod,
    /// Credits charged to `spent_credits_micro` and emitted as `actual_credits_micro`.
    pub committed_credits_micro: i64,
    /// Actual token usage added to the token telemetry counters (0 unless actual).
    pub telemetry_input_tokens: i64,
    pub telemetry_output_tokens: i64,
    /// `true` when actual tokens exceed the reserve (counted in `quota_overshoot`).
    pub overshoot: bool,
    pub overshoot_capped: bool,
}

/// Computes the settlement of a turn.
///
/// # Errors
/// Credit computation errors (out-of-range usage or multipliers).
pub fn settle(
    method: SettlementMethod,
    reserve: &TurnReserve,
    actual: Option<(i64, i64)>,
    in_mult: i64,
    out_mult: i64,
    overshoot_tolerance_factor: f64,
) -> Result<Settlement, CreditError> {
    match method {
        SettlementMethod::Released => Ok(Settlement {
            method,
            committed_credits_micro: 0,
            telemetry_input_tokens: 0,
            telemetry_output_tokens: 0,
            overshoot: false,
            overshoot_capped: false,
        }),
        SettlementMethod::Estimated => {
            let est_in = (reserve.reserve_tokens - reserve.max_output_tokens_applied).max(0);
            let charged_out = reserve.minimal_generation_floor_applied.max(0);
            let credits = credits_micro(est_in, charged_out, in_mult, out_mult)?;
            Ok(Settlement {
                method,
                committed_credits_micro: credits,
                telemetry_input_tokens: 0,
                telemetry_output_tokens: 0,
                overshoot: false,
                overshoot_capped: false,
            })
        }
        SettlementMethod::Actual => {
            let (inp, out) = actual.unwrap_or((0, 0));
            let actual_credits = credits_micro(inp, out, in_mult, out_mult)?;
            let actual_tokens = inp + out;
            let mut committed = actual_credits;
            let mut overshoot = false;
            let mut capped = false;
            if reserve.reserve_tokens > 0 && actual_tokens > reserve.reserve_tokens {
                overshoot = true;
                #[allow(clippy::cast_precision_loss)]
                let factor = actual_tokens as f64 / reserve.reserve_tokens as f64;
                if factor > overshoot_tolerance_factor {
                    committed = reserve.reserved_credits_micro;
                    capped = true;
                }
            }
            Ok(Settlement {
                method,
                committed_credits_micro: committed,
                telemetry_input_tokens: inp,
                telemetry_output_tokens: out,
                overshoot,
                overshoot_capped: capped,
            })
        }
    }
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod credits_tests;
