//! Credit arithmetic (DESIGN.md section 5.3).
//!
//! Credits are integer micro-credits (`1 credit = 1_000_000` `credits_micro`).
//! The canonical formula applies `ceil_div` per component.

/// Maximum token count accepted by the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Maximum credit multiplier accepted (micro-credits per 1M tokens).
pub const MAX_MULT: i64 = 10_000_000_000;

/// Errors of the checked credit computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[allow(clippy::field_names, reason = "error variant names are descriptive")] 
pub enum CreditError {
    /// Token count out of range (0..=`10_000_000`).
    #[error("token count out of range")]
    InvalidTokenCount,
    /// Multiplier out of range (1..=`10_000_000_000`). A zero multiplier is a
    /// separate, more specific error.
    #[error("zero credit multiplier")]
    ZeroMultiplier,
    /// Multiplier out of range (1..=`10_000_000_000`).
    #[error("credit multiplier out of range")]
    InvalidMultiplier,
    /// Arithmetic overflow.
    #[error("credit arithmetic overflow")]
    Overflow,
}

/// `ceil(n / d)` for non-negative integers.
#[allow(clippy::integer_division, reason = "the canonical formula is integer ceil division")]
fn ceil_div(n: i64, d: i64) -> i64 {
    (n / d) + i64::from(n % d != 0)
}

/// Canonical credit formula (per-component rounding):
/// `ceil_div(input * in_mult) + ceil_div(output * out_mult)` (per 1M tokens).
///
/// # Errors
///
/// Returns [`CreditError`] on out-of-range inputs, zero multipliers, or
/// overflow.
pub fn credits_micro_checked(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditError> {
    if !(0..=MAX_TOKENS).contains(&input_tokens) || !(0..=MAX_TOKENS).contains(&output_tokens) {
        return Err(CreditError::InvalidTokenCount);
    }
    if in_mult == 0 || out_mult == 0 {
        return Err(CreditError::ZeroMultiplier);
    }
    if !(1..=MAX_MULT).contains(&in_mult) || !(1..=MAX_MULT).contains(&out_mult) {
        return Err(CreditError::InvalidMultiplier);
    }
    let input_product = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditError::Overflow)?;
    let output_product = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditError::Overflow)?;
    let input_credits = ceil_div(input_product, 1_000_000);
    let output_credits = ceil_div(output_product, 1_000_000);
    input_credits
        .checked_add(output_credits)
        .ok_or(CreditError::Overflow)
}

/// Preflight variant: substitutes `i64::MAX` on any computation error, so a
/// broken candidate counts as unavailable (never fits its buckets).
#[must_use]
pub fn credits_micro_or_max(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> i64 {
    credits_micro_checked(input_tokens, output_tokens, in_mult, out_mult).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod tests;
