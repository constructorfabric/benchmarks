//! Preflight text estimate (DESIGN section 5.5.4).

use mini_chat_sdk::EstimationBudgets;

/// Conservative token estimate of a message of `utf8_bytes` bytes:
/// `ceil((ceil(bytes / max(bytes_per_token, 1)) + fixed_overhead) * (100 + margin) / 100)`, in
/// integer arithmetic. Saturates instead of overflowing.
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: usize, b: &EstimationBudgets) -> i64 {
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX);
    let bytes_per_token = i64::from(b.bytes_per_token_conservative.max(1));
    let base = ceil_div(bytes, bytes_per_token).saturating_add(i64::from(b.fixed_overhead_tokens));
    base.checked_mul(100 + i64::from(b.safety_margin_pct))
        .map_or(i64::MAX, |n| ceil_div(n, 100))
}

/// `ceil(n / d)` for `n >= 0`, `d > 0`.
#[allow(clippy::integer_division)] // rounded up explicitly
fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_examples() {
        let defaults = EstimationBudgets::default();
        assert_eq!(
            (
                defaults.bytes_per_token_conservative,
                defaults.fixed_overhead_tokens,
                defaults.safety_margin_pct
            ),
            (4, 100, 10)
        );
        assert_eq!(estimate_text_tokens(0, &defaults), 110);
        // ceil((ceil(10 / 4) + 100) * 110 / 100) = ceil(103 * 1.1) = ceil(113.3) = 114
        assert_eq!(estimate_text_tokens(10, &defaults), 114);
        // ceil((2 + 100) * 1.1) = ceil(112.2) = 113
        assert_eq!(estimate_text_tokens(8, &defaults), 113);
        let exact = EstimationBudgets {
            bytes_per_token_conservative: 1,
            fixed_overhead_tokens: 0,
            safety_margin_pct: 0,
            ..EstimationBudgets::default()
        };
        assert_eq!(estimate_text_tokens(1000, &exact), 1000);
        // A zero bytes-per-token from a foreign policy plugin is clamped to 1.
        let zero = EstimationBudgets {
            bytes_per_token_conservative: 0,
            ..exact
        };
        assert_eq!(estimate_text_tokens(7, &zero), 7);
        assert_eq!(estimate_text_tokens(usize::MAX, &defaults), i64::MAX);
    }
}
