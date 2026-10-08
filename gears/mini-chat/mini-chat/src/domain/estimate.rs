//! Token estimation and credit arithmetic (DESIGN §5.3, §5.5).

use mini_chat_sdk::EstimationBudgets;

pub const MAX_TOKENS: i64 = 10_000_000;
pub const MAX_MULT: i64 = 10_000_000_000;
const MICRO: i64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("invalid token count {0} (must be in 0..=10000000)")]
    InvalidTokenCount(i64),
    #[error("zero credit multiplier")]
    ZeroMultiplier,
    #[error("credit multiplier {0} out of range (1..=10000000000)")]
    InvalidMultiplier(i64),
    #[error("credit arithmetic overflow")]
    Overflow,
}

#[allow(
    clippy::integer_division,
    reason = "intentional integer arithmetic (explicit rounding)"
)]
fn ceil_div(n: i64, d: i64) -> i64 {
    let q = n / d;
    if n % d != 0 { q + 1 } else { q }
}

/// Canonical credit formula with per-component ceiling division and checked
/// arithmetic.
///
/// # Errors
/// Returns [`CreditError`] when an input is out of bounds or the computation
/// overflows.
pub fn credits_micro(
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
    let ip = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditError::Overflow)?;
    let op = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditError::Overflow)?;
    ceil_div(ip, MICRO)
        .checked_add(ceil_div(op, MICRO))
        .ok_or(CreditError::Overflow)
}

/// Conservative token estimate of a text of `bytes` UTF-8 bytes:
/// `ceil((ceil(bytes / bpt) + fixed_overhead) * (100 + margin) / 100)`.
#[must_use]
#[allow(
    clippy::integer_division,
    reason = "intentional integer arithmetic (explicit rounding)"
)]
pub fn estimate_text_tokens(bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX / 4);
    let base = ceil_div(bytes, bpt) + i64::from(b.fixed_overhead_tokens);
    ceil_div(
        base.saturating_mul(100 + i64::from(b.safety_margin_pct)),
        100,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credits_use_per_component_ceiling() {
        // 1 token at 1 micro-credit per 1M tokens -> ceil(1/1e6) = 1
        assert_eq!(credits_micro(1, 1, 1, 1).unwrap(), 2);
        assert_eq!(
            credits_micro(1000, 500, 2_500_000_000, 2_500_000_000).unwrap(),
            3_750_000
        );
        assert_eq!(credits_micro(0, 0, 5, 5).unwrap(), 0);
        assert_eq!(
            credits_micro(900, 300, 1_000_000_000, 1_000_000_000).unwrap(),
            1_200_000
        );
    }

    #[test]
    fn credits_reject_bad_inputs() {
        assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditError::ZeroMultiplier));
        assert!(matches!(
            credits_micro(1, 1, MAX_MULT + 1, 1),
            Err(CreditError::InvalidMultiplier(_))
        ));
        assert!(matches!(
            credits_micro(MAX_TOKENS + 1, 1, 1, 1),
            Err(CreditError::InvalidTokenCount(_))
        ));
        assert!(matches!(
            credits_micro(-1, 1, 1, 1),
            Err(CreditError::InvalidTokenCount(_))
        ));
    }

    #[test]
    fn text_estimate_formula() {
        let b = EstimationBudgets::default(); // bpt 4, overhead 100, margin 10
        // empty message -> overhead with margin = ceil(100 * 110 / 100) = 110
        assert_eq!(estimate_text_tokens(0, &b), 110);
        // 9 bytes -> ceil(9/4)=3 + 100 = 103 -> ceil(103*1.1)=114 (113.3)
        assert_eq!(estimate_text_tokens(9, &b), 114);
        let zero = EstimationBudgets {
            bytes_per_token_conservative: 0,
            ..EstimationBudgets::default()
        };
        assert_eq!(estimate_text_tokens(4, &zero), 115);
    }
}
