//! Token estimation without a tokenizer (DESIGN §5.5.4).

use mini_chat_sdk::EstimationBudgets;

/// `ceil((ceil(bytes / bpt) + fixed_overhead) * (100 + margin) / 100)`.
#[must_use]
#[allow(clippy::integer_division)] // reason: deliberate integer ceiling divisions per the estimation formula
pub fn estimate_text_tokens(utf8_bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX / 4);
    let base = (bytes + bpt - 1) / bpt + i64::from(b.fixed_overhead_tokens);
    let scaled = base.saturating_mul(100 + i64::from(b.safety_margin_pct));
    (scaled + 99) / 100
}

/// Estimate of a text item.
#[must_use]
pub fn estimate_str(s: &str, b: &EstimationBudgets) -> i64 {
    estimate_text_tokens(s.len(), b)
}

#[cfg(test)]
#[path = "estimate_tests.rs"]
mod tests;
