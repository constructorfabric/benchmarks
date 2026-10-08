//! Credit arithmetic and token estimation (DESIGN §5.3, §5.4.1, §5.5.4).

use mini_chat_sdk::EstimationBudgets;

/// Maximum token count accepted by the checked credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Maximum credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;
const MICRO: i64 = 1_000_000;

/// Credit computation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditsError {
    #[error("invalid token count: {0}")]
    InvalidTokenCount(i64),
    #[error("zero credit multiplier")]
    ZeroMultiplier,
    #[error("invalid credit multiplier: {0}")]
    InvalidMultiplier(i64),
    #[error("credit arithmetic overflow")]
    Overflow,
}

#[allow(clippy::integer_division, reason = "deliberate floor division as the basis of ceil division")]
fn ceil_div(n: i64, d: i64) -> i64 {
    let q = n / d;
    if n % d != 0 { q + 1 } else { q }
}

/// Canonical credit formula with per-component ceiling:
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
) -> Result<i64, CreditsError> {
    for t in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKENS).contains(&t) {
            return Err(CreditsError::InvalidTokenCount(t));
        }
    }
    for m in [in_mult, out_mult] {
        if m == 0 {
            return Err(CreditsError::ZeroMultiplier);
        }
        if !(1..=MAX_MULTIPLIER).contains(&m) {
            return Err(CreditsError::InvalidMultiplier(m));
        }
    }
    let a = input_tokens.checked_mul(in_mult).ok_or(CreditsError::Overflow)?;
    let b = output_tokens.checked_mul(out_mult).ok_or(CreditsError::Overflow)?;
    ceil_div(a, MICRO)
        .checked_add(ceil_div(b, MICRO))
        .ok_or(CreditsError::Overflow)
}

/// Conservative text token estimate of the current message:
/// `ceil((ceil(bytes / bpt) + fixed_overhead) * (100 + margin) / 100)`.
#[must_use]
#[allow(clippy::integer_division, reason = "deliberate saturation headroom constant")]
pub fn estimate_text_tokens(utf8_bytes: usize, budgets: &EstimationBudgets) -> i64 {
    let bpt = i64::from(budgets.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX / 4);
    let base = ceil_div(bytes, bpt) + i64::from(budgets.fixed_overhead_tokens);
    let scaled = base.saturating_mul(100 + i64::from(budgets.safety_margin_pct));
    ceil_div(scaled, 100)
}

/// Estimated token size of an arbitrary text block (context assembly).
#[must_use]
pub fn estimate_block_tokens(utf8_bytes: usize, budgets: &EstimationBudgets) -> i64 {
    estimate_text_tokens(utf8_bytes, budgets)
}

/// Surcharge flags of one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent per-tool surcharge flags")]
pub struct Surcharges {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
    pub images: u32,
}

impl Surcharges {
    /// Total surcharge tokens for these flags.
    #[must_use]
    pub fn tokens(self, budgets: &EstimationBudgets) -> i64 {
        let mut t = i64::from(self.images) * i64::from(budgets.image_token_budget);
        if self.file_search {
            t += i64::from(budgets.tool_surcharge_tokens);
        }
        if self.web_search {
            t += i64::from(budgets.web_search_surcharge_tokens);
        }
        if self.code_interpreter {
            t += i64::from(budgets.code_interpreter_surcharge_tokens);
        }
        t
    }

    /// Tool surcharges only (images excluded): the deductions of the context budget.
    #[must_use]
    pub fn tool_tokens(self, budgets: &EstimationBudgets) -> i64 {
        Self { images: 0, ..self }.tokens(budgets)
    }
}

/// Reserve of one candidate model (DESIGN §5.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveEstimate {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    /// `None` when the credits cannot be computed (treated as unavailable).
    pub reserved_credits_micro: Option<i64>,
}

/// Compute the reserve a candidate would book.
#[must_use]
pub fn reserve_estimate(
    message_bytes: usize,
    prior_context_tokens: i64,
    surcharges: Surcharges,
    budgets: &EstimationBudgets,
    max_output_tokens_applied: i64,
    in_mult: i64,
    out_mult: i64,
) -> ReserveEstimate {
    let estimated_input_tokens = estimate_text_tokens(message_bytes, budgets)
        .saturating_add(prior_context_tokens.max(0))
        .saturating_add(surcharges.tokens(budgets));
    let reserve_tokens = estimated_input_tokens.saturating_add(max_output_tokens_applied);
    let reserved_credits_micro =
        credits_micro(estimated_input_tokens, max_output_tokens_applied, in_mult, out_mult).ok();
    ReserveEstimate {
        estimated_input_tokens,
        max_output_tokens_applied,
        reserve_tokens,
        reserved_credits_micro,
    }
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod credits_tests;
