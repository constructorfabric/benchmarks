//! Canonical credit arithmetic (DESIGN §5.3).

/// Maximum token count accepted by the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Maximum credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;
const MICRO: i64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditsError {
    #[error("token count out of range (0..=10,000,000)")]
    InvalidTokenCount,
    #[error("credit multiplier is zero")]
    ZeroMultiplier,
    #[error("credit multiplier above 10,000,000,000")]
    MultiplierTooLarge,
    #[error("credit computation overflow")]
    Overflow,
}

/// `ceil(n / d)` for non-negative `n` and positive `d`.
#[must_use]
#[allow(clippy::integer_division)] // reason: deliberate integer ceiling division for credit math
pub fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

fn check_mult(m: i64) -> Result<(), CreditsError> {
    if m <= 0 {
        Err(CreditsError::ZeroMultiplier)
    } else if m > MAX_MULTIPLIER {
        Err(CreditsError::MultiplierTooLarge)
    } else {
        Ok(())
    }
}

/// `ceil_div(input * in_mult, 1e6) + ceil_div(output * out_mult, 1e6)`, checked.
///
/// # Errors
/// Out-of-range token counts or multipliers, or arithmetic overflow.
pub fn credits_micro_checked(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditsError> {
    if !(0..=MAX_TOKENS).contains(&input_tokens) || !(0..=MAX_TOKENS).contains(&output_tokens) {
        return Err(CreditsError::InvalidTokenCount);
    }
    check_mult(in_mult)?;
    check_mult(out_mult)?;
    let a = input_tokens.checked_mul(in_mult).ok_or(CreditsError::Overflow)?;
    let b = output_tokens.checked_mul(out_mult).ok_or(CreditsError::Overflow)?;
    ceil_div(a, MICRO)
        .checked_add(ceil_div(b, MICRO))
        .ok_or(CreditsError::Overflow)
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod tests;
