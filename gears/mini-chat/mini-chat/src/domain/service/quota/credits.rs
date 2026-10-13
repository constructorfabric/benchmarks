//! Checked credit arithmetic (DESIGN §5.3) and the preflight reserve estimate of one cascade
//! candidate (DESIGN §5.4.1, §5.5.4-§5.5.7).

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry};

use super::{PreflightInput, ToolGates};

/// Maximum token count accepted by the credit computation.
pub const MAX_TOKENS: i64 = 10_000_000;
/// Maximum credit multiplier (micro-credits per 1M tokens).
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;
/// `credits_micro` divisor (multipliers are per 1M tokens).
const PER_MILLION: i64 = 1_000_000;

/// Failure of the checked credit computation. Every variant names the offending value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("token count {0} is outside 0..={MAX_TOKENS}")]
    InvalidTokenCount(i64),
    #[error("credit multiplier is zero")]
    ZeroMultiplier,
    #[error("credit multiplier {0} is outside 1..={MAX_MULTIPLIER}")]
    InvalidMultiplier(i64),
    #[error("credit multiplication overflow")]
    MultiplicationOverflow,
    #[error("credit addition overflow")]
    AdditionOverflow,
}

fn check_tokens(t: i64) -> Result<(), CreditError> {
    if (0..=MAX_TOKENS).contains(&t) {
        Ok(())
    } else {
        Err(CreditError::InvalidTokenCount(t))
    }
}

fn check_mult(m: i64) -> Result<(), CreditError> {
    if m == 0 {
        Err(CreditError::ZeroMultiplier)
    } else if (1..=MAX_MULTIPLIER).contains(&m) {
        Ok(())
    } else {
        Err(CreditError::InvalidMultiplier(m))
    }
}

/// Ceil division of a non-negative numerator.
const fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + if n % d == 0 { 0 } else { 1 }
}

/// Canonical credit formula with per-component rounding:
/// `ceil(in * in_mult / 1e6) + ceil(out * out_mult / 1e6)`.
///
/// # Errors
/// [`CreditError`] when a token count or multiplier is out of range or the arithmetic overflows.
pub fn credits_micro(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditError> {
    check_tokens(input_tokens)?;
    check_tokens(output_tokens)?;
    check_mult(in_mult)?;
    check_mult(out_mult)?;
    let in_product = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditError::MultiplicationOverflow)?;
    let out_product = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditError::MultiplicationOverflow)?;
    ceil_div(in_product, PER_MILLION)
        .checked_add(ceil_div(out_product, PER_MILLION))
        .ok_or(CreditError::AdditionOverflow)
}

/// Credits of a catalog entry (its multipliers).
///
/// # Errors
/// See [`credits_micro`].
pub fn entry_credits_micro(
    entry: &ModelCatalogEntry,
    input_tokens: i64,
    output_tokens: i64,
) -> Result<i64, CreditError> {
    credits_micro(
        input_tokens,
        output_tokens,
        entry.input_tokens_credit_multiplier_micro,
        entry.output_tokens_credit_multiplier_micro,
    )
}

/// `estimated_text_tokens = ceil((ceil(bytes / bytes_per_token) + fixed_overhead) *
/// (100 + safety_margin_pct) / 100)`; `bytes_per_token_conservative = 0` is clamped to 1.
#[must_use]
pub fn estimate_text_tokens(utf8_bytes: usize, budgets: &EstimationBudgets) -> i64 {
    let bpt = u128::from(budgets.bytes_per_token_conservative.max(1));
    let bytes = u128::try_from(utf8_bytes).unwrap_or(u128::MAX / 4);
    let base = bytes.div_ceil(bpt) + u128::from(budgets.fixed_overhead_tokens);
    let with_margin = (base * (100 + u128::from(budgets.safety_margin_pct))).div_ceil(100);
    i64::try_from(with_margin).unwrap_or(i64::MAX)
}

/// Tools sent with `entry` for this request (gates of DESIGN §5.5.6).
#[must_use]
pub fn tool_gates(entry: &ModelCatalogEntry, input: &PreflightInput, kill: &KillSwitches) -> ToolGates {
    let support = entry.tool_support();
    ToolGates {
        web_search: input.web_search_requested && support.web_search,
        file_search: input.has_ready_documents && support.file_search && !kill.disable_file_search,
        code_interpreter: input.has_ready_code_interpreter_files
            && support.code_interpreter
            && !kill.disable_code_interpreter,
    }
}

/// The reserve a cascade candidate would book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateReserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i32,
    pub tools: ToolGates,
}

/// Computes the reserve of `entry` (its budgets, multipliers and output cap).
///
/// `streaming_max_output_tokens` is `streaming.max_output_tokens`; `minimal_generation_floor` is
/// the gear config `estimation_budgets.minimal_generation_floor`.
///
/// # Errors
/// [`CreditError`] when the credit computation fails (the candidate is then unavailable).
pub fn candidate_reserve(
    entry: &ModelCatalogEntry,
    input: &PreflightInput,
    kill: &KillSwitches,
    streaming_max_output_tokens: u32,
    minimal_generation_floor: u32,
) -> Result<CandidateReserve, CreditError> {
    let b = &entry.estimation_budgets;
    let tools = tool_gates(entry, input, kill);
    let mut est: i64 = estimate_text_tokens(input.message_bytes, b);
    let mut add = |v: i64| est = est.saturating_add(v);
    add(input.prior_context_tokens.max(0));
    add(i64::from(b.image_token_budget).saturating_mul(i64::from(input.image_count)));
    if tools.file_search {
        add(i64::from(b.tool_surcharge_tokens));
    }
    if tools.web_search {
        add(i64::from(b.web_search_surcharge_tokens));
    }
    if tools.code_interpreter {
        add(i64::from(b.code_interpreter_surcharge_tokens));
    }
    let max_out_u32 = entry.max_output_tokens.min(streaming_max_output_tokens);
    let max_output_tokens_applied = i32::try_from(max_out_u32).unwrap_or(i32::MAX);
    let floor = minimal_generation_floor.min(max_out_u32);
    let minimal_generation_floor_applied = i32::try_from(floor).unwrap_or(i32::MAX);
    let reserved_credits_micro =
        entry_credits_micro(entry, est, i64::from(max_output_tokens_applied))?;
    Ok(CandidateReserve {
        estimated_input_tokens: est,
        max_output_tokens_applied,
        reserve_tokens: est.saturating_add(i64::from(max_output_tokens_applied)),
        reserved_credits_micro,
        minimal_generation_floor_applied,
        tools,
    })
}
