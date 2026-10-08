//! Context plan assembly and truncation (DESIGN §4 "Context Plan Assembly
//! and Truncation").

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};

use super::credits::estimate_block_tokens;
use super::error::DomainError;

/// Preamble prepended to the thread summary in the next turn's context.
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// One history message (chronological input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

/// Input of the context planner.
#[derive(Debug, Clone)]
pub struct ContextInput<'a> {
    pub model: &'a ModelCatalogEntry,
    pub max_output_tokens_applied: i64,
    /// Tool surcharges (file search, web search, code interpreter) of the request.
    pub tool_surcharge_tokens: i64,
    pub instructions: &'a str,
    pub summary: Option<&'a str>,
    /// Recent messages, chronological (oldest first).
    pub recent: &'a [HistoryMessage],
    pub user_message: &'a str,
    pub images: u32,
}

/// Result of the context planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    /// Summary block to send (preamble + summary), when kept.
    pub summary_block: Option<String>,
    /// Estimated tokens of the kept summary block.
    pub summary_tokens: i64,
    /// Kept history (chronological).
    pub history: Vec<HistoryMessage>,
    /// At least one recent message was dropped.
    pub messages_truncated: bool,
    /// Estimated size of the assembled request.
    pub assembled_tokens: i64,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
}

/// Input token limit of the model: `min(max_input_tokens, context_window - mot)`
/// (`max_input_tokens = 0` means no separate limit).
#[must_use]
pub fn input_limit(model: &ModelCatalogEntry, mot: i64) -> i64 {
    let by_window = i64::from(model.context_window) - mot;
    if model.max_input_tokens > 0 {
        std::cmp::min(i64::from(model.max_input_tokens), by_window)
    } else {
        by_window
    }
}

fn est(text: &str, b: &EstimationBudgets) -> i64 {
    estimate_block_tokens(text.len(), b)
}

/// Build the context plan deterministically.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items do not fit.
pub fn plan(input: &ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let b = &input.model.estimation_budgets;
    let mot = input.max_output_tokens_applied;
    if mot >= i64::from(input.model.context_window) {
        return Err(DomainError::ContextBudgetExceeded(
            "max_output_tokens leaves no room for input in the model context window".to_owned(),
        ));
    }
    let limit = input_limit(input.model, mot);
    let budget = limit - input.tool_surcharge_tokens - i64::from(b.fixed_overhead_tokens);
    if budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded("tool surcharges exceed the input budget".to_owned()));
    }
    #[allow(clippy::suspicious_operation_groupings, reason = "false positive: image count times per-image budget")]
    let mandatory = est(input.instructions, b)
        + est(input.user_message, b)
        + i64::from(input.images) * i64::from(b.image_token_budget);
    if mandatory > budget {
        return Err(DomainError::ContextBudgetExceeded(format!(
            "the system prompt and the message need {mandatory} tokens, the budget is {budget}"
        )));
    }
    let mut remaining = budget - mandatory;
    let mut summary_block = None;
    let mut summary_tokens = 0;
    if let Some(s) = input.summary {
        let block = format!("{SUMMARY_PREAMBLE}\n\n{s}");
        let t = est(&block, b);
        if t <= remaining {
            remaining -= t;
            summary_tokens = t;
            summary_block = Some(block);
        }
    }
    let mut kept_rev: Vec<HistoryMessage> = Vec::new();
    let mut history_tokens = 0;
    let mut truncated = false;
    for (i, m) in input.recent.iter().enumerate().rev() {
        let t = est(&m.content, b);
        if t <= remaining {
            remaining -= t;
            history_tokens += t;
            kept_rev.push(m.clone());
        } else {
            truncated = truncated || input.recent[..=i].iter().any(|x| x.role != "system");
            break;
        }
    }
    let mut history: Vec<HistoryMessage> = kept_rev.into_iter().rev().collect();
    while history.first().is_some_and(|m| m.role == "assistant") {
        let m = history.remove(0);
        history_tokens -= est(&m.content, b);
        truncated = true;
    }
    Ok(ContextPlan {
        summary_block,
        summary_tokens,
        history,
        messages_truncated: truncated,
        assembled_tokens: mandatory + summary_tokens + history_tokens,
        effective_budget: limit,
    })
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod context_tests;
