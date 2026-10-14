//! Credit arithmetic (`cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic`): the one
//! canonical integer function of DESIGN 5.3 with per-component round-up division, bounds
//! checked before any multiplication and checked arithmetic after.

use thiserror::Error;
use toolkit_macros::domain_model;

/// The largest accepted token count (ten million).
pub const MAX_TOKEN_COUNT: i64 = 10_000_000;

/// The largest accepted multiplier (micro-credits per 1,000,000 tokens).
pub const MAX_MULTIPLIER_MICRO: u64 = 10_000_000_000;

/// Micro-credits per credit, and the divisor of the per-component rounding.
pub const MICRO_PER_CREDIT: i64 = 1_000_000;

/// [`MICRO_PER_CREDIT`] for the unsigned round-up division.
const MICRO_PER_CREDIT_UNSIGNED: u64 = 1_000_000;

/// Why a credit computation failed. At preflight the caller substitutes `i64::MAX` for the
/// reserve of the candidate; at settlement the finalization transaction fails.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CreditError {
    /// A token count is negative or above [`MAX_TOKEN_COUNT`]; names the offending count.
    #[error("invalid token count {tokens}")]
    InvalidTokenCount { tokens: i64 },
    /// A multiplier of 0 would make the usage free.
    #[error("zero credit multiplier")]
    ZeroMultiplier,
    /// A multiplier above [`MAX_MULTIPLIER_MICRO`]; names the offending multiplier.
    #[error("credit multiplier {multiplier} out of range")]
    MultiplierOutOfRange { multiplier: u64 },
    /// A checked multiplication overflowed.
    #[error("credit multiplication overflow")]
    MultiplicationOverflow,
    /// The checked sum of the two components overflowed.
    #[error("credit addition overflow")]
    AdditionOverflow,
}

/// `ceil_div(input_tokens * in_mult, 1_000_000) + ceil_div(output_tokens * out_mult, 1_000_000)`
/// with the token counts checked against `0..=10_000_000` first (input, then output), then the
/// multipliers against `1..=10_000_000_000` (zero and above-range are separate errors), all
/// before any multiplication. Rounding is per component, never on the sum.
///
/// # Errors
///
/// [`CreditError`] for an invalid token count or multiplier, or a checked-arithmetic overflow.
pub fn credits_micro(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: u64,
    out_mult: u64,
) -> Result<i64, CreditError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-token-bounds
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-no-parse-validation
    // the bounds are checked here, on every call, never when the provider response is parsed
    for tokens in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKEN_COUNT).contains(&tokens) {
            return Err(CreditError::InvalidTokenCount { tokens });
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-no-parse-validation
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-token-bounds
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-multiplier-bounds
    for multiplier in [in_mult, out_mult] {
        if multiplier == 0 {
            return Err(CreditError::ZeroMultiplier);
        }
        if multiplier > MAX_MULTIPLIER_MICRO {
            return Err(CreditError::MultiplierOutOfRange { multiplier });
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-multiplier-bounds
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-unit
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-totals-only
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-checked-multiply
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-ceil-per-component
    // a multiplier is micro-credits per 1,000,000 tokens; only the total input and output
    // counts enter (the cache and reasoning breakdowns apply no discount); each component is
    // multiplied with checked arithmetic and rounded up on its own, never on the sum
    let input = component(input_tokens, in_mult)?;
    let output = component(output_tokens, out_mult)?;
    let total = input
        .checked_add(output)
        .ok_or(CreditError::AdditionOverflow)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-ceil-per-component
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-checked-multiply
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-totals-only
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-unit
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-return
    Ok(total)
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-return
}

/// `ceil(tokens * multiplier / 1_000_000)` over validated inputs, with checked arithmetic.
fn component(tokens: i64, multiplier: u64) -> Result<i64, CreditError> {
    let tokens_unsigned =
        u64::try_from(tokens).map_err(|_| CreditError::InvalidTokenCount { tokens })?;
    let product = tokens_unsigned
        .checked_mul(multiplier)
        .ok_or(CreditError::MultiplicationOverflow)?;
    let units = product.div_ceil(MICRO_PER_CREDIT_UNSIGNED);
    i64::try_from(units).map_err(|_| CreditError::MultiplicationOverflow)
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod credits_tests;
