//! Preflight token estimation (DESIGN sections 5.4.1, 5.5), tool gates (section
//! 5.5.6) and the context window budget (section 2, "Context Window Budget").
//!
//! Every value comes from the model catalog entry's `estimation_budgets`; no
//! provider tokenizer is used. Arithmetic saturates: an absurd input yields a
//! huge estimate (which then fails the credit computation or the budget), never
//! a wrapped one.

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry};

use crate::domain::error::DomainError;

/// `ceil((ceil(bytes / bpt) + fixed_overhead) * (100 + margin) / 100)`, with
/// `bytes_per_token_conservative` clamped to at least 1. An empty text yields
/// `fixed_overhead_tokens` plus the margin.
#[must_use]
pub fn estimate_text_tokens(bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = u64::from(b.bytes_per_token_conservative.max(1));
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    let base = bytes
        .div_ceil(bpt)
        .saturating_add(u64::from(b.fixed_overhead_tokens));
    let scaled = base
        .saturating_mul(100 + u64::from(b.safety_margin_pct))
        .div_ceil(100);
    i64::try_from(scaled).unwrap_or(i64::MAX)
}

/// `min(model.max_output_tokens, streaming.max_output_tokens)`.
#[must_use]
pub fn max_output_applied(model: &ModelCatalogEntry, cap: u32) -> i64 {
    i64::from(model.max_output_tokens.min(cap))
}

/// `min(max_input_tokens, context_window - max_output_applied)`;
/// `max_input_tokens = 0` means no separate input limit. May be `<= 0`.
#[must_use]
pub fn effective_input_budget(model: &ModelCatalogEntry, max_output_applied: i64) -> i64 {
    let window = i64::from(model.context_window).saturating_sub(max_output_applied);
    if model.max_input_tokens > 0 {
        window.min(i64::from(model.max_input_tokens))
    } else {
        window
    }
}

/// Token budget for the assembled context: the effective input budget minus
/// `surcharges` (tool, web search and code interpreter surcharges of the tools
/// sent) minus the model's `fixed_overhead_tokens`.
///
/// # Errors
/// `ContextBudgetExceeded` when `max_output_applied >= context_window` or the
/// deductions reach the input budget.
pub fn token_budget(
    model: &ModelCatalogEntry,
    max_output_applied: i64,
    surcharges: i64,
) -> Result<i64, DomainError> {
    if max_output_applied >= i64::from(model.context_window) {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let budget = effective_input_budget(model, max_output_applied)
        .saturating_sub(surcharges)
        .saturating_sub(i64::from(model.estimation_budgets.fixed_overhead_tokens));
    if budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }
    Ok(budget)
}

/// Per-turn facts that decide which tools are sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolContext {
    /// The chat has at least one ready document.
    pub chat_has_ready_documents: bool,
    /// The chat has at least one ready code-interpreter (XLSX) attachment.
    pub chat_has_ready_ci_files: bool,
    /// The request has `web_search.enabled = true`.
    pub web_search_requested: bool,
}

/// Which provider tools a model would be sent with. The same gates decide the
/// reserve surcharges, the daily tool quota checks and the tool list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolGates {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// DESIGN section 5.5.6: `file_search` with a ready document, catalog support
/// and `disable_file_search` off; `web_search` when requested, supported and
/// `disable_web_search` off; `code_interpreter` with a ready XLSX attachment,
/// catalog support and `disable_code_interpreter` off.
#[must_use]
#[allow(clippy::trivially_copy_pass_by_ref)] // signature fixed by the plan interface
pub fn tool_gates(model: &ModelCatalogEntry, ks: &KillSwitches, ctx: &ToolContext) -> ToolGates {
    let support = &model.general_config.tool_support;
    ToolGates {
        file_search: support.file_search && !ks.disable_file_search && ctx.chat_has_ready_documents,
        web_search: support.web_search && !ks.disable_web_search && ctx.web_search_requested,
        code_interpreter: support.code_interpreter
            && !ks.disable_code_interpreter
            && ctx.chat_has_ready_ci_files,
    }
}

/// Sum of the reserve surcharges for the tools in `gates` (section 5.5.6).
#[must_use]
pub fn tool_surcharges(gates: ToolGates, b: &EstimationBudgets) -> i64 {
    let mut total = 0_i64;
    if gates.file_search {
        total += i64::from(b.tool_surcharge_tokens);
    }
    if gates.web_search {
        total += i64::from(b.web_search_surcharge_tokens);
    }
    if gates.code_interpreter {
        total += i64::from(b.code_interpreter_surcharge_tokens);
    }
    total
}

#[cfg(test)]
#[path = "estimation_tests.rs"]
mod estimation_tests;
