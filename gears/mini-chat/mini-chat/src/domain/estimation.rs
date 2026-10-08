//! Credit arithmetic (D§5.3) and the preflight reserve estimate (D§5.4.1,
//! D§5.5).

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry};

/// Upper bound of a token count in a credit computation (inclusive).
pub const MAX_TOKENS: i64 = 10_000_000;
/// Upper bound of a credit multiplier (inclusive); the lower bound is 1.
pub const MAX_MULTIPLIER: i64 = 10_000_000_000;

/// A credit computation that cannot be done (D§5.3 "Overflow Protection").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditError {
    #[error("token count {0} outside 0..={MAX_TOKENS}")]
    TokenCountOutOfRange(i64),
    #[error("credit multiplier is zero")]
    ZeroMultiplier,
    #[error("credit multiplier {0} outside 1..={MAX_MULTIPLIER}")]
    MultiplierOutOfRange(i64),
    #[error("credit computation overflow")]
    Overflow,
}

/// `ceil_div(input * in_mult, 1e6) + ceil_div(output * out_mult, 1e6)`,
/// rounded per component, with bounds and checked arithmetic.
///
/// # Errors
/// `CreditError` when a token count or multiplier is out of range or the
/// arithmetic overflows.
pub fn credits_micro(
    input: i64,
    output: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditError> {
    for tokens in [input, output] {
        if !(0..=MAX_TOKENS).contains(&tokens) {
            return Err(CreditError::TokenCountOutOfRange(tokens));
        }
    }
    for mult in [in_mult, out_mult] {
        if mult == 0 {
            return Err(CreditError::ZeroMultiplier);
        }
        if !(1..=MAX_MULTIPLIER).contains(&mult) {
            return Err(CreditError::MultiplierOutOfRange(mult));
        }
    }
    let component = |tokens: i64, mult: i64| {
        tokens
            .checked_mul(mult)
            .map(|n| ceil_div(n, 1_000_000))
            .ok_or(CreditError::Overflow)
    };
    component(input, in_mult)?
        .checked_add(component(output, out_mult)?)
        .ok_or(CreditError::Overflow)
}

/// `ceil(n / d)` for `n >= 0`, `d > 0`.
#[allow(
    clippy::integer_division,
    reason = "ceil division is the normative rounding"
)]
const fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + if n % d == 0 { 0 } else { 1 }
}

/// Credit multipliers `(in, out)` of a catalog entry as `i64` (a value
/// above `i64::MAX` is out of range).
///
/// # Errors
/// `CreditError::MultiplierOutOfRange` when a multiplier does not fit `i64`.
pub fn multipliers(m: &ModelCatalogEntry) -> Result<(i64, i64), CreditError> {
    let conv = |v: u64| i64::try_from(v).map_err(|_| CreditError::MultiplierOutOfRange(i64::MAX));
    Ok((
        conv(m.input_tokens_credit_multiplier_micro)?,
        conv(m.output_tokens_credit_multiplier_micro)?,
    ))
}

/// Conservative token estimate of a message of `bytes` UTF-8 bytes:
/// `ceil((ceil(bytes / bpt) + overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    let base = ceil_div(bytes, bpt).saturating_add(i64::from(b.fixed_overhead_tokens));
    let margin = 100 + i64::from(b.safety_margin_pct);
    ceil_div(base.saturating_mul(margin), 100)
}

/// Request facts the reserve estimate depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReserveInputs {
    /// UTF-8 bytes of the user message.
    pub message_bytes: usize,
    /// `input_tokens + output_tokens` of the latest non-deleted assistant
    /// message with non-zero tokens (0 when none).
    pub prior_context_tokens: i64,
    pub image_count: u32,
    /// The chat has at least one ready document attachment.
    pub chat_has_ready_docs: bool,
    /// The chat has at least one ready code-interpreter (XLSX) attachment.
    pub chat_has_ready_xlsx: bool,
    /// `web_search.enabled` of the request.
    pub web_search_requested: bool,
}

/// Tools the provider request carries for a model (the same gates decide
/// the surcharges).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolGates {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Reserve a candidate model would book (D§5.3.1 preflight variables).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservePlan {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: i64,
    /// `credits_micro(estimated_input_tokens, max_output_tokens_applied, ..)`;
    /// `i64::MAX` when it cannot be computed (the candidate never fits).
    pub reserved_credits_micro: i64,
    /// `min(config floor, max_output_tokens_applied)`.
    pub minimal_generation_floor_applied: i64,
    pub tools: ToolGates,
}

/// Tool gates of model `m` for the request (D§5.5.6).
#[must_use]
#[allow(clippy::trivially_copy_pass_by_ref, reason = "plan signature")]
pub fn tool_gates(m: &ModelCatalogEntry, i: &ReserveInputs, ks: &KillSwitches) -> ToolGates {
    let support = &m.general_config.tool_support;
    ToolGates {
        file_search: i.chat_has_ready_docs && support.file_search && !ks.disable_file_search,
        web_search: i.web_search_requested && support.web_search && !ks.disable_web_search,
        code_interpreter: i.chat_has_ready_xlsx
            && support.code_interpreter
            && !ks.disable_code_interpreter,
    }
}

/// Reserve of candidate model `m` (its estimation budgets and multipliers;
/// `cfg_max_out` = `streaming.max_output_tokens`, `cfg_floor` =
/// `estimation_budgets.minimal_generation_floor`).
#[must_use]
#[allow(clippy::trivially_copy_pass_by_ref, reason = "plan signature")]
pub fn candidate_reserve(
    m: &ModelCatalogEntry,
    i: &ReserveInputs,
    ks: &KillSwitches,
    cfg_max_out: u32,
    cfg_floor: u32,
) -> ReservePlan {
    let b = &m.estimation_budgets;
    let tools = tool_gates(m, i, ks);
    let surcharge = |on: bool, tokens: u32| if on { i64::from(tokens) } else { 0 };
    let estimated_input_tokens = estimate_text_tokens(i.message_bytes, b)
        .saturating_add(i.prior_context_tokens)
        .saturating_add(i64::from(i.image_count) * i64::from(b.image_token_budget))
        .saturating_add(surcharge(tools.file_search, b.tool_surcharge_tokens))
        .saturating_add(surcharge(tools.web_search, b.web_search_surcharge_tokens))
        .saturating_add(surcharge(
            tools.code_interpreter,
            b.code_interpreter_surcharge_tokens,
        ));
    let max_output_tokens_applied = i64::from(m.max_output_tokens.min(cfg_max_out));
    let reserved_credits_micro = multipliers(m)
        .and_then(|(in_mult, out_mult)| {
            credits_micro(
                estimated_input_tokens,
                max_output_tokens_applied,
                in_mult,
                out_mult,
            )
        })
        .unwrap_or_else(|e| {
            tracing::warn!(model = %m.id, error = %e, "reserve cannot be computed; candidate unavailable");
            i64::MAX
        });
    ReservePlan {
        estimated_input_tokens,
        max_output_tokens_applied,
        reserve_tokens: estimated_input_tokens.saturating_add(max_output_tokens_applied),
        reserved_credits_micro,
        minimal_generation_floor_applied: i64::from(cfg_floor).min(max_output_tokens_applied),
        tools,
    }
}

#[cfg(test)]
#[path = "estimation_tests.rs"]
mod tests;
