//! Credit arithmetic (DESIGN §5.3): integer micro-credits with per-component
//! ceiling division and checked bounds.

use thiserror::Error;

/// Maximum token count accepted by the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Maximum credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;

/// Credit computation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CreditsError {
    #[error("token count out of range: {0}")]
    InvalidTokenCount(i64),
    #[error("credit multiplier is zero")]
    ZeroMultiplier,
    #[error("credit multiplier out of range: {0}")]
    MultiplierOutOfRange(i64),
    #[error("credit arithmetic overflow")]
    Overflow,
}

#[allow(clippy::integer_division)] // truncating quotient is intended; remainder handled below
fn ceil_div(n: i64, d: i64) -> i64 {
    let q = n / d;
    if n % d != 0 { q + 1 } else { q }
}

fn check_mult(m: i64) -> Result<(), CreditsError> {
    if m == 0 {
        return Err(CreditsError::ZeroMultiplier);
    }
    if !(1..=MAX_MULTIPLIER).contains(&m) {
        return Err(CreditsError::MultiplierOutOfRange(m));
    }
    Ok(())
}

/// `ceil(input * in_mult / 1e6) + ceil(output * out_mult / 1e6)`.
///
/// # Errors
/// Out-of-range tokens or multipliers, or overflow.
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
    check_mult(in_mult)?;
    check_mult(out_mult)?;
    let a = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditsError::Overflow)?;
    let b = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditsError::Overflow)?;
    ceil_div(a, 1_000_000)
        .checked_add(ceil_div(b, 1_000_000))
        .ok_or(CreditsError::Overflow)
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod credits_tests;
