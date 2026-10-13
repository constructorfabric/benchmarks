//! Credit arithmetic (DESIGN section 5.3): integer micro-credits with per-component rounding.

use thiserror::Error;

/// Largest token count a credit computation accepts.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Largest credit multiplier a credit computation accepts (micro-credits per one million tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;

const MICRO: i64 = 1_000_000;

/// Why a credit computation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CreditsError {
    /// A token count is negative or above [`MAX_TOKENS`].
    #[error("token count {0} is outside 0..={MAX_TOKENS}")]
    InvalidTokenCount(i64),
    /// A multiplier is zero, negative or above [`MAX_MULTIPLIER`].
    #[error("credit multiplier {0} is outside 1..={MAX_MULTIPLIER}")]
    InvalidMultiplier(i64),
    /// The arithmetic overflowed.
    #[error("credit arithmetic overflow")]
    Overflow,
}

#[allow(
    clippy::integer_division,
    reason = "the credit formula is defined on integer micro-credits with an explicit ceiling"
)]
fn ceil_div(numerator: i64, denominator: i64) -> i64 {
    numerator / denominator + i64::from(numerator % denominator != 0)
}

fn component(tokens: i64, multiplier: i64) -> Result<i64, CreditsError> {
    if !(0..=MAX_TOKENS).contains(&tokens) {
        return Err(CreditsError::InvalidTokenCount(tokens));
    }
    if !(1..=MAX_MULTIPLIER).contains(&multiplier) {
        return Err(CreditsError::InvalidMultiplier(multiplier));
    }
    tokens
        .checked_mul(multiplier)
        .map(|product| ceil_div(product, MICRO))
        .ok_or(CreditsError::Overflow)
}

/// `ceil(input * in_mult / 1e6) + ceil(output * out_mult / 1e6)`, validated and overflow-checked.
///
/// # Errors
/// [`CreditsError`] when a token count or a multiplier is out of range or the sum overflows.
pub fn credits_micro(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditsError> {
    component(input_tokens, in_mult)?
        .checked_add(component(output_tokens, out_mult)?)
        .ok_or(CreditsError::Overflow)
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod tests;
