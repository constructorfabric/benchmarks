//! Context budget (`cpt-cf-mini-chat-algo-message-streaming-context-budget`).

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_macros::domain_model;

use crate::domain::error::DomainError;
use crate::domain::streaming::tools::{ToolKind, ToolSet};

/// The token budgets of a turn.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// `input_limit` minus the deductions: what the assembled context must fit in.
    pub token_budget: i64,
    /// `input_limit` (no surcharge and no fixed overhead subtracted): the base of the
    /// thread-summary threshold.
    pub effective_budget: i64,
}

/// The budgets of the effective model: `input_limit = context_window - max_output_tokens_applied`
/// lowered to `max_input_tokens` when that is not 0 and smaller; the deductions are
/// `fixed_overhead_tokens` plus the surcharge of each tool of the tool set (`file_search`:
/// `tool_surcharge_tokens`, `web_search`: `web_search_surcharge_tokens`, `code_interpreter`:
/// `code_interpreter_surcharge_tokens`, `search_knowledge`: none).
///
/// # Errors
///
/// [`DomainError::ContextBudgetExceeded`] when `max_output_tokens_applied` is at least
/// `context_window`, or when the deductions reach `input_limit`.
pub fn compute_budget(
    entry: &ModelCatalogEntry,
    max_output_tokens_applied: i64,
    tools: &ToolSet,
) -> Result<ContextBudget, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-no-room
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-return-no-room
    let window = i64::from(entry.context_window);
    if max_output_tokens_applied >= window {
        return Err(DomainError::ContextBudgetExceeded);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-return-no-room
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-no-room
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-input-limit
    let mut input_limit = window - max_output_tokens_applied;
    if entry.max_input_tokens != 0 {
        input_limit = input_limit.min(i64::from(entry.max_input_tokens));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-input-limit
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-deductions
    // `search_knowledge` has no surcharge
    let budgets = &entry.estimation_budgets;
    let deductions = tools
        .tools
        .iter()
        .map(|tool| match tool.kind {
            ToolKind::FileSearch => i64::from(budgets.tool_surcharge_tokens),
            ToolKind::WebSearch => i64::from(budgets.web_search_surcharge_tokens),
            ToolKind::CodeInterpreter => i64::from(budgets.code_interpreter_surcharge_tokens),
            ToolKind::SearchKnowledge => 0,
        })
        .fold(
            i64::from(budgets.fixed_overhead_tokens),
            i64::saturating_add,
        );
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-deductions
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-deductions-exceed
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-return-deductions
    if deductions >= input_limit {
        return Err(DomainError::ContextBudgetExceeded);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-return-deductions
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-deductions-exceed
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-values
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-return
    // the effective budget (the base of the thread-summary threshold) subtracts neither the
    // surcharges nor the fixed overhead
    Ok(ContextBudget {
        token_budget: input_limit - deductions,
        effective_budget: input_limit,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-budget:p1:inst-budget-values
}
