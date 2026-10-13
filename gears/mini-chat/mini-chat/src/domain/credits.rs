//! Credit arithmetic (DESIGN section 5.3).
//!
//! `credits_micro` is the integer accounting unit: `1 credit = 1_000_000`
//! micro-credits. Every call site uses [`credits_micro_checked`], which applies
//! `ceil_div` per component (input and output separately, never to the sum).

/// Largest accepted token count per component.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Largest accepted multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;

const MICRO: i64 = 1_000_000;

/// Why a credit computation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    /// A token count is negative or above [`MAX_TOKENS`].
    #[error("token count out of range 0..={MAX_TOKENS}")]
    InvalidTokenCount,
    /// A multiplier is zero (or negative): that usage would be free.
    #[error("credit multiplier must be positive")]
    ZeroMultiplier,
    /// A multiplier is above [`MAX_MULTIPLIER`].
    #[error("credit multiplier above {MAX_MULTIPLIER}")]
    MultiplierTooLarge,
    /// `tokens * multiplier` overflowed `i64`.
    #[error("credit multiplication overflow")]
    MultiplicationOverflow,
    /// The sum of the two components overflowed `i64`.
    #[error("credit addition overflow")]
    AdditionOverflow,
}

/// `credits_micro(input, output, in_mult, out_mult)` with bounds checks and
/// checked arithmetic.
///
/// # Errors
///
/// Returns a [`CreditError`] before multiplying when a token count is outside
/// `0..=10_000_000` or a multiplier is outside `1..=10_000_000_000`, and on any
/// arithmetic overflow.
pub fn credits_micro_checked(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditError> {
    for tokens in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKENS).contains(&tokens) {
            return Err(CreditError::InvalidTokenCount);
        }
    }
    for mult in [in_mult, out_mult] {
        if mult <= 0 {
            return Err(CreditError::ZeroMultiplier);
        }
        if mult > MAX_MULTIPLIER {
            return Err(CreditError::MultiplierTooLarge);
        }
    }
    let input = component(input_tokens, in_mult)?;
    let output = component(output_tokens, out_mult)?;
    input
        .checked_add(output)
        .ok_or(CreditError::AdditionOverflow)
}

fn component(tokens: i64, mult: i64) -> Result<i64, CreditError> {
    let product = tokens
        .checked_mul(mult)
        .ok_or(CreditError::MultiplicationOverflow)?;
    // Both factors are in range and positive, so `product` is non-negative.
    let micro = u64::try_from(MICRO).map_err(|_| CreditError::MultiplicationOverflow)?;
    let product = u64::try_from(product).map_err(|_| CreditError::MultiplicationOverflow)?;
    i64::try_from(product.div_ceil(micro)).map_err(|_| CreditError::MultiplicationOverflow)
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod credits_tests;
