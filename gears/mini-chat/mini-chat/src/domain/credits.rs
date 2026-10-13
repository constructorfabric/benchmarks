//! Credit arithmetic (DESIGN §5.3 "Credit Arithmetic" and "Overflow Protection").

/// Maximum accepted token count per component.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Maximum accepted credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;

const MICRO: i64 = 1_000_000;

/// Reason a credit computation was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditsError {
    #[error("token count outside 0..=10000000")]
    InvalidTokenCount,
    #[error("credit multiplier must be positive")]
    ZeroMultiplier,
    #[error("credit multiplier above 10000000000")]
    MultiplierTooLarge,
    #[error("credit arithmetic overflow")]
    Overflow,
}

fn check_multiplier(m: i64) -> Result<(), CreditsError> {
    if m <= 0 {
        Err(CreditsError::ZeroMultiplier)
    } else if m > MAX_MULTIPLIER {
        Err(CreditsError::MultiplierTooLarge)
    } else {
        Ok(())
    }
}

fn component(tokens: i64, mult: i64) -> Result<i64, CreditsError> {
    let product = tokens.checked_mul(mult).ok_or(CreditsError::Overflow)?;
    // `product` is non-negative here (tokens >= 0, multiplier > 0).
    #[allow(clippy::integer_division)]
    Ok(product / MICRO + i64::from(product % MICRO != 0))
}

/// `ceil_div(input * in_mult, 1e6) + ceil_div(output * out_mult, 1e6)` with
/// range validation and checked arithmetic.
///
/// # Errors
///
/// Returns a [`CreditsError`] when a token count is outside `0..=10_000_000`,
/// a multiplier is outside `1..=10_000_000_000`, or the arithmetic overflows.
pub fn credits_micro(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditsError> {
    for t in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKENS).contains(&t) {
            return Err(CreditsError::InvalidTokenCount);
        }
    }
    check_multiplier(in_mult)?;
    check_multiplier(out_mult)?;
    component(input_tokens, in_mult)?
        .checked_add(component(output_tokens, out_mult)?)
        .ok_or(CreditsError::Overflow)
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod credits_tests;
