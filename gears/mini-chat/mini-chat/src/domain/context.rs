//! Context window budget, token estimation and context truncation
//! (DESIGN 2.2 "Context Window Budget", 4 "Context Plan Truncation Algorithm",
//! 5.5.4 "Input Token Estimation").
//!
//! Everything here is pure: the stream service feeds the functions with the
//! catalog entry of the effective model and the recent messages of the chat and
//! gets back a deterministic plan.

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};

/// Tools that are part of one provider request; each one deducts its surcharge
/// from the input budget.
#[derive(Debug, Clone, Copy, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per built-in tool; the shape is part of the context API"
)]
pub struct ToolSet {
    /// `file_search` is sent.
    pub file_search: bool,
    /// `web_search` is sent.
    pub web_search: bool,
    /// `code_interpreter` is sent.
    pub code_interpreter: bool,
}

/// Role of a history message (system messages are never part of the history).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRole {
    /// Written by the user.
    User,
    /// Produced by the model.
    Assistant,
}

/// One recent message of the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    /// Author role.
    pub role: HistoryRole,
    /// Stored content, verbatim.
    pub content: String,
}

/// Why a context could not be planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextError {
    /// The current message alone exceeds the model's `max_input_tokens`.
    InputTooLong,
    /// The mandatory items (system prompt, current message, images) do not fit
    /// the input budget.
    BudgetExceeded,
}

/// Input of [`plan_context`].
pub struct ContextInput<'a> {
    /// Result of [`token_budget`].
    pub token_budget: u64,
    /// Estimation budgets of the effective model.
    pub budgets: &'a EstimationBudgets,
    /// System prompt including the tool guards.
    pub system_prompt: &'a str,
    /// Recent messages, chronological (already limited to K).
    pub history: &'a [HistoryMessage],
    /// The current user message, verbatim.
    pub user_message: &'a str,
    /// Number of images of the current turn.
    pub image_count: u32,
}

/// The planned context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    /// Kept history, chronological; never starts with an assistant message.
    pub history: Vec<HistoryMessage>,
    /// At least one history message was dropped.
    pub messages_truncated: bool,
    /// Estimated size of the mandatory items plus the kept history.
    pub estimated_tokens: u64,
}

/// `ceil((ceil(bytes / bytes_per_token) + overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_tokens(text: &str, budgets: &EstimationBudgets) -> u64 {
    let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    let per_token = u64::from(budgets.bytes_per_token_conservative.max(1));
    let base = bytes
        .div_ceil(per_token)
        .saturating_add(u64::from(budgets.fixed_overhead_tokens));
    base.saturating_mul(100 + u64::from(budgets.safety_margin_pct))
        .div_ceil(100)
}

/// Input token budget of a request:
/// `min(max_input_tokens, context_window - max_output_tokens_applied)` minus
/// the surcharges of the tools in the request and the fixed overhead.
///
/// `None` when nothing is left for the input.
#[must_use]
pub fn token_budget(
    entry: &ModelCatalogEntry,
    max_output_tokens_applied: u32,
    tools: ToolSet,
) -> Option<u64> {
    let window = u64::from(entry.context_window);
    let applied = u64::from(max_output_tokens_applied);
    if applied >= window {
        return None;
    }
    let mut input_limit = window - applied;
    if entry.max_input_tokens != 0 {
        input_limit = input_limit.min(u64::from(entry.max_input_tokens));
    }
    let budgets = &entry.estimation_budgets;
    let mut deductions = u64::from(budgets.fixed_overhead_tokens);
    if tools.file_search {
        deductions = deductions.saturating_add(u64::from(budgets.tool_surcharge_tokens));
    }
    if tools.web_search {
        deductions = deductions.saturating_add(u64::from(budgets.web_search_surcharge_tokens));
    }
    if tools.code_interpreter {
        deductions =
            deductions.saturating_add(u64::from(budgets.code_interpreter_surcharge_tokens));
    }
    (deductions < input_limit).then(|| input_limit - deductions)
}

/// Rejects a message whose estimate exceeds the model's `max_input_tokens`
/// (`0` = no separate limit).
///
/// # Errors
/// [`ContextError::InputTooLong`] when the estimate is above the limit.
pub fn check_input_length(entry: &ModelCatalogEntry, content: &str) -> Result<(), ContextError> {
    if entry.max_input_tokens == 0 {
        return Ok(());
    }
    if estimate_tokens(content, &entry.estimation_budgets) > u64::from(entry.max_input_tokens) {
        return Err(ContextError::InputTooLong);
    }
    Ok(())
}

/// Delivered to the model in front of the thread summary (DESIGN B.5.5): the
/// earlier messages were replaced by the summary, the recent ones follow.
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// The text of the summary message: the preamble, a blank line, the summary.
#[must_use]
pub fn summary_message(summary: &str) -> String {
    format!("{SUMMARY_PREAMBLE}\n\n{summary}")
}

/// The context with the thread summary placed in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPlan {
    /// The planned history (planned in what the summary leaves).
    pub context: ContextPlan,
    /// The summary is part of the context.
    pub summary_kept: bool,
    /// Estimated size of everything the request carries: mandatory items, the
    /// summary message when kept, the kept history.
    pub assembled_tokens: u64,
}

fn mandatory_tokens(input: &ContextInput<'_>) -> u64 {
    let budgets = input.budgets;
    estimate_tokens(input.system_prompt, budgets)
        .saturating_add(estimate_tokens(input.user_message, budgets))
        .saturating_add(
            u64::from(input.image_count).saturating_mul(u64::from(budgets.image_token_budget)),
        )
}

/// [`plan_context`] with the thread summary (DESIGN 4 "Context Plan Truncation
/// Algorithm"): the mandatory items come first; the summary (preamble
/// included) is kept when it fits next to them and is dropped entirely
/// otherwise; the history is then planned in what the summary leaves, so the
/// summary ranks above it.
///
/// # Errors
/// [`ContextError::BudgetExceeded`] when the mandatory items alone do not fit.
pub fn plan_context_with_summary(
    input: &ContextInput<'_>,
    summary: Option<&str>,
) -> Result<SummaryPlan, ContextError> {
    let mandatory = mandatory_tokens(input);
    let summary_cost = summary
        .map(|text| estimate_tokens(&summary_message(text), input.budgets))
        .filter(|cost| mandatory.saturating_add(*cost) <= input.token_budget);
    let reserved = summary_cost.unwrap_or(0);
    let context = plan_context(&ContextInput {
        token_budget: input.token_budget.saturating_sub(reserved),
        budgets: input.budgets,
        system_prompt: input.system_prompt,
        history: input.history,
        user_message: input.user_message,
        image_count: input.image_count,
    })?;
    let assembled_tokens = context.estimated_tokens.saturating_add(reserved);
    Ok(SummaryPlan {
        context,
        summary_kept: summary_cost.is_some(),
        assembled_tokens,
    })
}

/// Plans the context: the mandatory items must fit the budget, the history is
/// walked newest to oldest and kept while it fits, and the kept range never
/// starts with an assistant answer.
///
/// # Errors
/// [`ContextError::BudgetExceeded`] when the mandatory items alone do not fit.
pub fn plan_context(input: &ContextInput<'_>) -> Result<ContextPlan, ContextError> {
    let budgets = input.budgets;
    let mandatory = mandatory_tokens(input);
    if mandatory > input.token_budget {
        return Err(ContextError::BudgetExceeded);
    }

    let mut used = mandatory;
    let mut keep_from = input.history.len();
    for (index, message) in input.history.iter().enumerate().rev() {
        let cost = estimate_tokens(&message.content, budgets);
        if used.saturating_add(cost) > input.token_budget {
            break;
        }
        used = used.saturating_add(cost);
        keep_from = index;
    }
    // A range never starts with an answer, also when everything fits.
    while input
        .history
        .get(keep_from)
        .is_some_and(|message| message.role == HistoryRole::Assistant)
    {
        used = used.saturating_sub(estimate_tokens(&input.history[keep_from].content, budgets));
        keep_from += 1;
    }

    Ok(ContextPlan {
        history: input.history[keep_from..].to_vec(),
        messages_truncated: keep_from > 0,
        estimated_tokens: used,
    })
}
