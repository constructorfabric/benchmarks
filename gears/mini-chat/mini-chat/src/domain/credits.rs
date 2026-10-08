//! Credit arithmetic (DESIGN §5.3) and preflight token estimation (§5.4.1, §5.5.4).

use mini_chat_sdk::EstimationBudgets;

/// Maximum token count accepted by the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Maximum credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;

/// Failure of the checked credit computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    /// A token count is negative or above [`MAX_TOKENS`].
    #[error("invalid token count {0}")]
    InvalidTokenCount(i64),
    /// A multiplier is zero.
    #[error("zero credit multiplier")]
    ZeroMultiplier,
    /// A multiplier is negative or above [`MAX_MULTIPLIER`].
    #[error("invalid credit multiplier {0}")]
    InvalidMultiplier(i64),
    /// Arithmetic overflow.
    #[error("credit arithmetic overflow")]
    Overflow,
}

/// Truncating division rounded up when there is a remainder (`d` is never zero at call sites).
fn ceil_div(n: i64, d: i64) -> i64 {
    n.checked_div(d).unwrap_or(0) + i64::from(n % d != 0)
}

/// `ceil(input*in/1e6) + ceil(output*out/1e6)` with bounds checks.
///
/// # Errors
/// See [`CreditError`].
pub fn credits_micro_checked(
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
        if !(1..=MAX_MULTIPLIER).contains(&m) {
            return Err(CreditError::InvalidMultiplier(m));
        }
    }
    let a = input_tokens.checked_mul(in_mult).ok_or(CreditError::Overflow)?;
    let b = output_tokens.checked_mul(out_mult).ok_or(CreditError::Overflow)?;
    ceil_div(a, 1_000_000)
        .checked_add(ceil_div(b, 1_000_000))
        .ok_or(CreditError::Overflow)
}

/// Converts an unsigned catalog multiplier to `i64` (saturating; the checked
/// computation rejects values above the maximum).
#[must_use]
pub fn mult(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// `ceil((ceil(bytes / bpt) + overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(utf8_bytes).unwrap_or(i64::MAX >> 2);
    let base = ceil_div(bytes, bpt) + i64::from(b.fixed_overhead_tokens);
    ceil_div(base * (100 + i64::from(b.safety_margin_pct)), 100)
}

/// Tools that are sent with a candidate model (decided before estimation).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one independent flag per contract-defined tool (file_search, web_search, code_interpreter)"
)]
pub struct ToolFlags {
    /// `file_search` is sent.
    pub file_search: bool,
    /// `web_search` is sent.
    pub web_search: bool,
    /// `code_interpreter` is sent.
    pub code_interpreter: bool,
}

/// Inputs of the preflight reserve.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReserveInputs {
    /// UTF-8 bytes of the current message.
    pub message_bytes: usize,
    /// `input + output` tokens of the latest assistant message with usage.
    pub prior_context_tokens: i64,
    /// Images on the current message.
    pub image_count: u32,
    /// Tools sent.
    pub tools: ToolFlags,
}

/// Preflight reserve of one candidate model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserve {
    /// Estimated input tokens (text + prior context + surcharges).
    pub estimated_input_tokens: i64,
    /// `min(catalog max_output_tokens, streaming.max_output_tokens)`.
    pub max_output_tokens_applied: i64,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    #[allow(clippy::struct_field_names, reason = "mirrors the `chat_turns.reserve_tokens` column of the data model")]
    pub reserve_tokens: i64,
    /// Credits of the reserve (`i64::MAX` when not computable).
    pub reserved_credits_micro: i64,
}

/// Computes the reserve of a candidate (DESIGN §5.4.1).
#[must_use]
pub fn compute_reserve(
    inputs: &ReserveInputs,
    budgets: &EstimationBudgets,
    catalog_max_output_tokens: u32,
    streaming_max_output_tokens: u32,
    in_mult: u64,
    out_mult: u64,
) -> Reserve {
    let mut est = estimate_text_tokens(inputs.message_bytes, budgets)
        .saturating_add(inputs.prior_context_tokens.max(0))
        .saturating_add(i64::from(inputs.image_count) * i64::from(budgets.image_token_budget));
    if inputs.tools.file_search {
        est = est.saturating_add(i64::from(budgets.tool_surcharge_tokens));
    }
    if inputs.tools.web_search {
        est = est.saturating_add(i64::from(budgets.web_search_surcharge_tokens));
    }
    if inputs.tools.code_interpreter {
        est = est.saturating_add(i64::from(budgets.code_interpreter_surcharge_tokens));
    }
    let max_out = i64::from(catalog_max_output_tokens.min(streaming_max_output_tokens));
    let credits = credits_micro_checked(est, max_out, mult(in_mult), mult(out_mult))
        .unwrap_or(i64::MAX);
    Reserve {
        estimated_input_tokens: est,
        max_output_tokens_applied: max_out,
        reserve_tokens: est.saturating_add(max_out),
        reserved_credits_micro: credits,
    }
}

/// Estimated settlement (DESIGN §5.8): input estimate plus the generation floor.
///
/// # Errors
/// See [`CreditError`].
pub fn estimated_settlement_credits(
    reserve_tokens: i64,
    max_output_tokens_applied: i64,
    floor_applied: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditError> {
    let est_input = (reserve_tokens - max_output_tokens_applied).max(0);
    credits_micro_checked(est_input, floor_applied.max(0), in_mult, out_mult)
}

#[cfg(test)]
#[path = "credits_tests.rs"]
mod credits_tests;
