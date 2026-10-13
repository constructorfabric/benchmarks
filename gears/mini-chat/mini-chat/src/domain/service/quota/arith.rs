//! Integer credit arithmetic
//! (`cpt-cf-mini-chat-algo-quota-billing-credits-micro`,
//! `cpt-cf-mini-chat-dod-quota-billing-credit-arithmetic`).

use thiserror::Error;

/// Largest accepted token count of one component.
pub const MAX_TOKEN_COUNT: i64 = 10_000_000;

/// Largest accepted credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER_MICRO: u64 = 10_000_000_000;

/// Micro-credits per token-million divisor of the multipliers.
pub const MICRO: i64 = 1_000_000;

/// Failure of a credit computation; every variant names the offending value.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CreditError {
    /// A token count is negative or above [`MAX_TOKEN_COUNT`]; `field` is
    /// `input_tokens` or `output_tokens`.
    #[error("invalid token count: {field} = {value}")]
    InvalidTokenCount {
        /// `input_tokens` or `output_tokens`.
        field: &'static str,
        /// The offending value.
        value: i64,
    },
    /// A multiplier is 0 (it would make the usage free); `field` is `in_mult`
    /// or `out_mult`.
    #[error("zero multiplier: {field}")]
    ZeroMultiplier {
        /// `in_mult` or `out_mult`.
        field: &'static str,
    },
    /// A multiplier is above [`MAX_MULTIPLIER_MICRO`] or does not fit a signed
    /// 64-bit integer; `field` is `in_mult` or `out_mult`.
    #[error("invalid multiplier: {field} = {value}")]
    InvalidMultiplier {
        /// `in_mult` or `out_mult`.
        field: &'static str,
        /// The offending value.
        value: u64,
    },
    /// A product overflowed 64 bits.
    #[error("credit multiplication overflow")]
    MultiplicationOverflow,
    /// The sum of the two components overflowed 64 bits.
    #[error("credit addition overflow")]
    AdditionOverflow,
}

/// `credits_micro(input_tokens, output_tokens, in_mult, out_mult)`: the
/// ceiling of `input_tokens * in_mult / 1_000_000` plus the ceiling of
/// `output_tokens * out_mult / 1_000_000` (the ceiling is applied per
/// component, never to the sum), in checked 64-bit integer arithmetic.
///
/// # Errors
///
/// [`CreditError`] naming the offending token count or multiplier.
// @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-credit-arithmetic:p1
pub fn credits_micro(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: u64,
    out_mult: u64,
) -> Result<i64, CreditError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-check-input-tokens
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-input-tokens
    check_tokens("input_tokens", input_tokens)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-input-tokens
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-check-input-tokens
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-check-output-tokens
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-output-tokens
    check_tokens("output_tokens", output_tokens)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-output-tokens
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-check-output-tokens
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-check-zero-mult
    if in_mult == 0 {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-zero-mult
        return Err(CreditError::ZeroMultiplier { field: "in_mult" });
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-zero-mult
    }
    if out_mult == 0 {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-zero-mult
        return Err(CreditError::ZeroMultiplier { field: "out_mult" });
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-zero-mult
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-check-zero-mult
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-check-mult-range
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-mult-range
    let in_mult = checked_multiplier("in_mult", in_mult)?;
    let out_mult = checked_multiplier("out_mult", out_mult)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-mult-range
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-check-mult-range
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-multiply
    let input_product = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditError::MultiplicationOverflow)?;
    let output_product = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditError::MultiplicationOverflow)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-multiply
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-ceil-per-component
    let input_credits = ceil_micro(input_product)?;
    let output_credits = ceil_micro(output_product)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-ceil-per-component
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-add-components
    let credits = input_credits
        .checked_add(output_credits)
        .ok_or(CreditError::AdditionOverflow)?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-add-components
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-credits
    Ok(credits)
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-credits-micro:p1:inst-return-credits
}

fn check_tokens(field: &'static str, value: i64) -> Result<(), CreditError> {
    if (0..=MAX_TOKEN_COUNT).contains(&value) {
        Ok(())
    } else {
        Err(CreditError::InvalidTokenCount { field, value })
    }
}

fn checked_multiplier(field: &'static str, value: u64) -> Result<i64, CreditError> {
    if value > MAX_MULTIPLIER_MICRO {
        return Err(CreditError::InvalidMultiplier { field, value });
    }
    i64::try_from(value).map_err(|_| CreditError::InvalidMultiplier { field, value })
}

/// Ceiling of `product / 1_000_000` for a non-negative product.
fn ceil_micro(product: i64) -> Result<i64, CreditError> {
    let divisor = u64::try_from(MICRO).map_err(|_| CreditError::AdditionOverflow)?;
    let product = u64::try_from(product).map_err(|_| CreditError::MultiplicationOverflow)?;
    i64::try_from(product.div_ceil(divisor)).map_err(|_| CreditError::AdditionOverflow)
}

#[cfg(test)]
#[path = "arith_tests.rs"]
mod tests;
