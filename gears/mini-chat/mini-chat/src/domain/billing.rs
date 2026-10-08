//! Pure credit arithmetic, token estimation and billing-outcome derivation
//! (DESIGN §5.3–§5.9).

use mini_chat_sdk::EstimationBudgets;

/// Upper bound of a token count accepted by the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Upper bound of a credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;
const MICRO: i64 = 1_000_000;

/// Credit computation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("token count out of range: {0}")]
    InvalidTokenCount(i64),
    #[error("credit multiplier is zero")]
    ZeroMultiplier,
    #[error("credit multiplier out of range: {0}")]
    InvalidMultiplier(i64),
    #[error("credit arithmetic overflow")]
    Overflow,
}

fn ceil_div(n: i64, d: i64) -> i64 {
    let q = n.div_euclid(d);
    if n.rem_euclid(d) == 0 { q } else { q + 1 }
}

/// Canonical credit formula with per-component ceiling division:
/// `ceil(input * in_mult / 1e6) + ceil(output * out_mult / 1e6)`.
///
/// # Errors
/// Token counts outside `0..=10_000_000`, a zero multiplier, a multiplier
/// above `10_000_000_000`, or an arithmetic overflow.
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
        if !(1..=MAX_MULTIPLIER).contains(&m) {
            return Err(CreditError::InvalidMultiplier(m));
        }
    }
    let input_product = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditError::Overflow)?;
    let output_product = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditError::Overflow)?;
    ceil_div(input_product, MICRO)
        .checked_add(ceil_div(output_product, MICRO))
        .ok_or(CreditError::Overflow)
}

/// Conservative token estimate of a text:
/// `ceil((ceil(bytes / bpt) + fixed_overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: usize, budgets: &EstimationBudgets) -> i64 {
    let bpt = i64::from(budgets.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX >> 2);
    let base = ceil_div(bytes, bpt) + i64::from(budgets.fixed_overhead_tokens);
    ceil_div(
        base.saturating_mul(100 + i64::from(budgets.safety_margin_pct)),
        100,
    )
}

/// Plain token estimate of a context item (no fixed overhead, no margin):
/// `ceil(bytes / bpt)`.
#[must_use]
pub fn estimate_plain_tokens(utf8_bytes: usize, budgets: &EstimationBudgets) -> i64 {
    let bpt = i64::from(budgets.bytes_per_token_conservative.max(1));
    ceil_div(i64::try_from(utf8_bytes).unwrap_or(i64::MAX >> 2), bpt)
}

/// Inputs of the preflight reserve of one candidate model.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReserveInputs {
    pub message_bytes: usize,
    pub prior_context_tokens: i64,
    pub image_count: u32,
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Preflight reserve of a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct Reserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
}

/// Compute the reserve a candidate model would book.
///
/// # Errors
/// Propagates [`CreditError`] from the credit computation.
pub fn compute_reserve(
    inputs: &ReserveInputs,
    budgets: &EstimationBudgets,
    max_output_tokens_applied: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<Reserve, CreditError> {
    let mut estimated = estimate_text_tokens(inputs.message_bytes, budgets)
        .saturating_add(inputs.prior_context_tokens.max(0))
        .saturating_add(i64::from(inputs.image_count) * i64::from(budgets.image_token_budget));
    if inputs.file_search {
        estimated = estimated.saturating_add(i64::from(budgets.tool_surcharge_tokens));
    }
    if inputs.web_search {
        estimated = estimated.saturating_add(i64::from(budgets.web_search_surcharge_tokens));
    }
    if inputs.code_interpreter {
        estimated = estimated.saturating_add(i64::from(budgets.code_interpreter_surcharge_tokens));
    }
    let reserved_credits_micro =
        credits_micro(estimated, max_output_tokens_applied, in_mult, out_mult)?;
    Ok(Reserve {
        estimated_input_tokens: estimated,
        max_output_tokens_applied,
        reserve_tokens: estimated.saturating_add(max_output_tokens_applied),
        reserved_credits_micro,
    })
}

/// How a turn's quota reserve is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementMethod {
    Actual,
    Estimated,
    Released,
}

impl SettlementMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

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

/// Error codes classified as pre-provider failures (settled `released`).
pub const PRE_PROVIDER_CODES: &[&str] = &[
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// Error codes of failures after the provider call started.
pub const POST_PROVIDER_CODES: &[&str] = &[
    "provider_error",
    "provider_timeout",
    "rate_limited",
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];

/// Shared billing-outcome derivation (DESIGN §5.8 normative table).
///
/// `usage_known` must be `true` only when the provider reported usage; for
/// failed turns it additionally requires at least one non-zero count.
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
            Some(code) if PRE_PROVIDER_CODES.contains(&code) => {
                (BillingOutcome::Failed, SettlementMethod::Released)
            }
            Some(code) if POST_PROVIDER_CODES.contains(&code) => {
                if usage_known {
                    (BillingOutcome::Failed, SettlementMethod::Actual)
                } else {
                    (BillingOutcome::Failed, SettlementMethod::Estimated)
                }
            }
            other => {
                tracing::error!(error_code = ?other, "mini-chat: unknown turn error code at settlement");
                (BillingOutcome::Failed, SettlementMethod::Estimated)
            }
        },
    }
}

/// Persisted reserve fields of a turn (read back at settlement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistedReserve {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
}

/// Charge of a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charge {
    /// Credits debited (committed credits).
    pub credits_micro: i64,
    /// `true` when completed actual usage exceeded the tolerance and the
    /// charge was capped at the reserve.
    pub overshoot_capped: bool,
    /// `true` when actual tokens exceeded the reserve.
    pub overshoot: bool,
}

/// Settle with provider-reported usage, applying the overshoot cap.
///
/// # Errors
/// Propagates [`CreditError`].
pub fn settle_actual(
    reserve: &PersistedReserve,
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
    tolerance: f64,
) -> Result<Charge, CreditError> {
    let actual_credits = credits_micro(input_tokens, output_tokens, in_mult, out_mult)?;
    let actual_tokens = input_tokens.saturating_add(output_tokens);
    let mut charge = Charge {
        credits_micro: actual_credits,
        overshoot_capped: false,
        overshoot: false,
    };
    if reserve.reserve_tokens > 0 && actual_tokens > reserve.reserve_tokens {
        charge.overshoot = true;
        #[allow(clippy::cast_precision_loss)]
        let factor = actual_tokens as f64 / reserve.reserve_tokens as f64;
        if factor > tolerance {
            charge.credits_micro = reserve.reserved_credits_micro;
            charge.overshoot_capped = true;
        }
    }
    Ok(charge)
}

/// Deterministic estimated settlement:
/// `credits(estimated_input, minimal_generation_floor_applied)`.
///
/// # Errors
/// Propagates [`CreditError`].
pub fn settle_estimated(
    reserve: &PersistedReserve,
    in_mult: i64,
    out_mult: i64,
) -> Result<Charge, CreditError> {
    let estimated_input = (reserve.reserve_tokens - reserve.max_output_tokens_applied).max(0);
    let credits = credits_micro(
        estimated_input,
        reserve.minimal_generation_floor_applied.max(0),
        in_mult,
        out_mult,
    )?;
    Ok(Charge {
        credits_micro: credits,
        overshoot_capped: false,
        overshoot: false,
    })
}

/// Remaining percentage (floored, 0..=100) of a period.
#[must_use]
pub fn remaining_percentage(limit: i64, used: i64) -> u8 {
    if limit <= 0 {
        return 0;
    }
    let remaining = (limit - used).max(0);
    let pct = remaining.saturating_mul(100).div_euclid(limit);
    u8::try_from(pct.clamp(0, 100)).unwrap_or(0)
}

/// `(warning, exhausted)` flags for a remaining percentage.
#[must_use]
pub fn warning_flags(remaining_pct: u8, warning_threshold_pct: u8) -> (bool, bool) {
    let warning =
        u16::from(remaining_pct) <= 100u16.saturating_sub(u16::from(warning_threshold_pct));
    (warning, remaining_pct == 0)
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod billing_tests;
