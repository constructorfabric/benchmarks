//! Canonical credit arithmetic (DESIGN section 5.3).

use thiserror::Error;

/// Upper bound for a token count fed into the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Upper bound (inclusive) for a credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER_MICRO: i64 = 10_000_000_000;

const MICRO: i64 = 1_000_000;

/// Failure of the checked credit computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CreditsError {
    #[error("token count out of range 0..=10000000")]
    InvalidTokenCount,
    #[error("credit multiplier must be positive")]
    ZeroMultiplier,
    #[error("credit multiplier exceeds 10000000000")]
    MultiplierTooLarge,
    #[error("credit computation overflowed")]
    Overflow,
}

/// Compute `ceil_div(input * in_mult, 1e6) + ceil_div(output * out_mult, 1e6)`.
///
/// Each component is rounded up separately. Tokens must be in
/// `0..=10_000_000` and multipliers in `1..=10_000_000_000`; anything else is
/// rejected before multiplying.
///
/// # Errors
///
/// Returns a [`CreditsError`] naming the violated bound, or
/// [`CreditsError::Overflow`] if a checked operation overflows.
pub fn credits_micro_checked(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditsError> {
    for tokens in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKENS).contains(&tokens) {
            return Err(CreditsError::InvalidTokenCount);
        }
    }
    for mult in [in_mult, out_mult] {
        if mult <= 0 {
            return Err(CreditsError::ZeroMultiplier);
        }
        if mult > MAX_MULTIPLIER_MICRO {
            return Err(CreditsError::MultiplierTooLarge);
        }
    }
    // Bounds were validated above; ceil-division of a non-negative product.
    #[allow(clippy::integer_division)]
    let component = |tokens: i64, mult: i64| -> Result<i64, CreditsError> {
        let product = tokens.checked_mul(mult).ok_or(CreditsError::Overflow)?;
        Ok(product / MICRO + i64::from(product % MICRO != 0))
    };
    component(input_tokens, in_mult)?
        .checked_add(component(output_tokens, out_mult)?)
        .ok_or(CreditsError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_component_ceil_div() {
        assert_eq!(credits_micro_checked(1, 1, 1_500_000, 1_500_000), Ok(4));
        assert_eq!(
            credits_micro_checked(1000, 500, 2_500_000_000, 2_500_000_000),
            Ok(3_750_000)
        );
    }

    #[test]
    fn bounds() {
        assert_eq!(
            credits_micro_checked(10_000_001, 1, 1, 1),
            Err(CreditsError::InvalidTokenCount)
        );
        assert_eq!(
            credits_micro_checked(1, 10_000_001, 1, 1),
            Err(CreditsError::InvalidTokenCount)
        );
        assert_eq!(
            credits_micro_checked(-1, 1, 1, 1),
            Err(CreditsError::InvalidTokenCount)
        );
        assert_eq!(
            credits_micro_checked(1, 1, 0, 1),
            Err(CreditsError::ZeroMultiplier)
        );
        assert_eq!(
            credits_micro_checked(1, 1, 1, 0),
            Err(CreditsError::ZeroMultiplier)
        );
        assert_eq!(
            credits_micro_checked(1, 1, 10_000_000_001, 1),
            Err(CreditsError::MultiplierTooLarge)
        );
        assert_eq!(
            credits_micro_checked(1, 1, 1, 10_000_000_001),
            Err(CreditsError::MultiplierTooLarge)
        );
        // Upper bounds are inclusive and must not overflow.
        assert!(
            credits_micro_checked(10_000_000, 10_000_000, 10_000_000_000, 10_000_000_000).is_ok()
        );
    }
}
