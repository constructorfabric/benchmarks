//! Conservative token estimation without a tokenizer (DESIGN §5.5.4).

use mini_chat_sdk::EstimationBudgets;

/// `ceil((ceil(bytes / bpt) + fixed_overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(text: &str, b: &EstimationBudgets) -> i64 {
    estimate_bytes_tokens(text.len(), b)
}

/// Same as [`estimate_text_tokens`] for a byte count.
#[must_use]
pub fn estimate_bytes_tokens(bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = u64::from(b.bytes_per_token_conservative.max(1));
    let bytes = bytes as u64;
    let base = bytes.div_ceil(bpt) + u64::from(b.fixed_overhead_tokens);
    let with_margin = (base * (100 + u64::from(b.safety_margin_pct))).div_ceil(100);
    i64::try_from(with_margin).unwrap_or(i64::MAX)
}

/// Token size of a context item without the fixed per-request overhead:
/// `ceil(ceil(bytes / bpt) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_item_tokens(bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = u64::from(b.bytes_per_token_conservative.max(1));
    let base = (bytes as u64).div_ceil(bpt);
    let with_margin = (base * (100 + u64::from(b.safety_margin_pct))).div_ceil(100);
    i64::try_from(with_margin).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_message_is_fixed_overhead_plus_margin() {
        let b = EstimationBudgets::default(); // 4 bytes/token, 100 overhead, 10%
        assert_eq!(estimate_text_tokens("", &b), 110);
    }

    #[test]
    fn rounding_is_conservative() {
        let b = EstimationBudgets::default();
        // 5 bytes -> ceil(5/4)=2 -> 102 * 1.1 = 112.2 -> 113
        assert_eq!(estimate_text_tokens("hello", &b), 113);
        let zero_bpt = EstimationBudgets {
            bytes_per_token_conservative: 0,
            ..EstimationBudgets::default()
        };
        assert_eq!(estimate_text_tokens("ab", &zero_bpt), 113);
        assert_eq!(estimate_item_tokens(8, &b), 3);
    }
}
