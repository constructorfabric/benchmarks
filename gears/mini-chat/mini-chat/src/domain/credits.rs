//! Credit arithmetic and token estimation (DESIGN §5.3, §5.4.1, §5.5).

use mini_chat_sdk::EstimationBudgets;

pub const MAX_TOKENS: i64 = 10_000_000;
pub const MAX_MULT: i64 = 10_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("invalid token count {0}")]
    InvalidTokenCount(i64),
    #[error("zero multiplier")]
    ZeroMultiplier,
    #[error("invalid multiplier {0}")]
    InvalidMultiplier(i64),
    #[error("overflow")]
    Overflow,
}

fn ceil_div_1e6(n: i64) -> i64 {
    let d = 1_000_000_i64;
    (n / d) + i64::from(n % d != 0)
}

/// Canonical credit formula with per-component ceil rounding and checked
/// arithmetic.
///
/// # Errors
/// On out-of-range token counts or multipliers, or overflow.
pub fn credits_micro_checked(
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
    let a = input_tokens.checked_mul(in_mult).ok_or(CreditError::Overflow)?;
    let b = output_tokens.checked_mul(out_mult).ok_or(CreditError::Overflow)?;
    ceil_div_1e6(a)
        .checked_add(ceil_div_1e6(b))
        .ok_or(CreditError::Overflow)
}

/// `ceil(a / b)` for non-negative integers.
#[must_use]
pub fn ceil_div_u64(a: u64, b: u64) -> u64 {
    if b == 0 {
        return a;
    }
    a.div_ceil(b)
}

/// Estimated tokens of a text: `ceil((ceil(bytes / bpt) + overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(text: &str, b: &EstimationBudgets) -> i64 {
    let bpt = u64::from(b.bytes_per_token_conservative.max(1));
    let bytes = text.len() as u64;
    let base = ceil_div_u64(bytes, bpt) + u64::from(b.fixed_overhead_tokens);
    let with_margin = ceil_div_u64(base * (100 + u64::from(b.safety_margin_pct)), 100);
    i64::try_from(with_margin).unwrap_or(i64::MAX)
}

/// Estimated tokens of a context item (no fixed overhead, margin applied).
#[must_use]
pub fn estimate_item_tokens(text: &str, b: &EstimationBudgets) -> i64 {
    let bpt = u64::from(b.bytes_per_token_conservative.max(1));
    let base = ceil_div_u64(text.len() as u64, bpt);
    let with_margin = ceil_div_u64(base * (100 + u64::from(b.safety_margin_pct)), 100);
    i64::try_from(with_margin).unwrap_or(i64::MAX)
}

/// Which surcharges apply to a reserve estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SurchargeFlags {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Inputs and outputs of the preflight reserve calculation (§5.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveEstimate {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
}

/// Reserve a candidate model would book. A computation error yields
/// `reserved_credits_micro = i64::MAX` (candidate unavailable).
#[must_use]
pub fn reserve_for(
    message: &str,
    prior_context_tokens: i64,
    image_count: usize,
    flags: SurchargeFlags,
    budgets: &EstimationBudgets,
    model_max_output: u32,
    cfg_max_output: u32,
    in_mult: i64,
    out_mult: i64,
) -> ReserveEstimate {
    let text = estimate_text_tokens(message, budgets);
    let images = i64::try_from(image_count).unwrap_or(0) * i64::from(budgets.image_token_budget);
    let mut est = text.saturating_add(prior_context_tokens.max(0)).saturating_add(images);
    if flags.file_search {
        est = est.saturating_add(i64::from(budgets.tool_surcharge_tokens));
    }
    if flags.web_search {
        est = est.saturating_add(i64::from(budgets.web_search_surcharge_tokens));
    }
    if flags.code_interpreter {
        est = est.saturating_add(i64::from(budgets.code_interpreter_surcharge_tokens));
    }
    let max_out = i64::from(model_max_output.min(cfg_max_output));
    let reserved =
        credits_micro_checked(est, max_out, in_mult, out_mult).unwrap_or(i64::MAX);
    ReserveEstimate {
        estimated_input_tokens: est,
        max_output_tokens_applied: max_out,
        reserve_tokens: est.saturating_add(max_out),
        reserved_credits_micro: reserved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_per_component_rounding() {
        assert_eq!(credits_micro_checked(1, 1, 1, 1).unwrap(), 2);
        assert_eq!(credits_micro_checked(1000, 500, 2_500_000_000, 2_500_000_000).unwrap(), 3_750_000);
        assert_eq!(credits_micro_checked(100, 50, 1_000_000, 3_000_000).unwrap(), 250);
        assert_eq!(credits_micro_checked(0, 0, 1, 1).unwrap(), 0);
    }

    #[test]
    fn credits_rejects_bad_inputs() {
        assert_eq!(credits_micro_checked(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
        assert!(matches!(credits_micro_checked(MAX_TOKENS + 1, 1, 1, 1), Err(CreditError::InvalidTokenCount(_))));
        assert!(matches!(credits_micro_checked(1, 1, MAX_MULT + 1, 1), Err(CreditError::InvalidMultiplier(_))));
    }

    #[test]
    fn text_estimate_matches_formula() {
        let b = EstimationBudgets::default(); // 4 bpt, 100 overhead, 10 %
        // 8 bytes -> 2 + 100 = 102 -> ceil(102 * 110 / 100) = 113
        assert_eq!(estimate_text_tokens("12345678", &b), 113);
        assert_eq!(estimate_text_tokens("", &b), 110);
    }

    #[test]
    fn reserve_applies_surcharges_and_cap() {
        let b = EstimationBudgets::default();
        let r = reserve_for(
            "12345678",
            10,
            1,
            SurchargeFlags { file_search: true, web_search: true, code_interpreter: false },
            &b,
            4096,
            1000,
            1_000_000,
            1_000_000,
        );
        assert_eq!(r.estimated_input_tokens, 113 + 10 + 1000 + 500 + 500);
        assert_eq!(r.max_output_tokens_applied, 1000);
        assert_eq!(r.reserve_tokens, r.estimated_input_tokens + 1000);
        assert_eq!(r.reserved_credits_micro, r.reserve_tokens);
    }

    #[test]
    fn reserve_error_is_unavailable() {
        let b = EstimationBudgets::default();
        let r = reserve_for("x", 0, 0, SurchargeFlags::default(), &b, 10, 10, 0, 1);
        assert_eq!(r.reserved_credits_micro, i64::MAX);
    }
}
