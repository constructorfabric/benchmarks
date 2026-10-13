//! Context plan assembly and deterministic truncation (DESIGN §4 "Context Plan
//! Assembly and Truncation").

use mini_chat_sdk::EstimationBudgets;

use crate::domain::error::DomainError;
use crate::domain::estimate::estimate_item_tokens;

pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// A history message offered to context assembly (chronological order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

/// Input of [`assemble`].
#[derive(Debug, Clone)]
pub struct ContextInput<'a> {
    pub instructions: String,
    pub summary: Option<&'a str>,
    pub history: Vec<HistoryMessage>,
    pub user_message: &'a str,
    pub image_count: usize,
    pub budgets: &'a EstimationBudgets,
    pub context_window: u32,
    pub max_input_tokens: u32,
    pub max_output_tokens_applied: u32,
    /// Sum of the tool / web search / code interpreter surcharges in the request.
    pub surcharges: i64,
}

/// Assembled context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    pub instructions: String,
    /// Summary message text (preamble + summary) when kept.
    pub summary_message: Option<String>,
    pub summary_tokens: i64,
    pub messages: Vec<HistoryMessage>,
    pub messages_truncated: bool,
    pub assembled_tokens: i64,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
}

/// Input limit: `min(max_input_tokens, context_window - max_output_tokens_applied)`
/// (`max_input_tokens = 0`: no separate limit; `context_window = 0`: unbounded).
///
/// # Errors
/// `ContextBudgetExceeded` when the output cap leaves no input room.
pub fn input_limit(
    context_window: u32,
    max_input_tokens: u32,
    max_output_tokens_applied: u32,
) -> Result<i64, DomainError> {
    let by_window = if context_window == 0 {
        i64::MAX / 4
    } else {
        if max_output_tokens_applied >= context_window {
            return Err(DomainError::ContextBudgetExceeded);
        }
        i64::from(context_window) - i64::from(max_output_tokens_applied)
    };
    Ok(if max_input_tokens > 0 {
        by_window.min(i64::from(max_input_tokens))
    } else {
        by_window
    })
}

/// Assembles the context plan.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items do not fit.
pub fn assemble(input: ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let limit = input_limit(
        input.context_window,
        input.max_input_tokens,
        input.max_output_tokens_applied,
    )?;
    let budget = limit - input.surcharges - i64::from(input.budgets.fixed_overhead_tokens);
    if budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let est = |s: &str| estimate_item_tokens(s.len(), input.budgets);

    let images = i64::try_from(input.image_count).unwrap_or(i64::MAX / 4)
        * i64::from(input.budgets.image_token_budget);
    let mandatory = est(&input.instructions) + est(input.user_message) + images;
    if mandatory > budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut remaining = budget - mandatory;

    let mut summary_message = None;
    let mut summary_tokens = 0;
    if let Some(summary) = input.summary {
        let text = format!("{SUMMARY_PREAMBLE}\n\n{summary}");
        let t = est(&text);
        if t <= remaining {
            remaining -= t;
            summary_tokens = t;
            summary_message = Some(text);
        }
    }

    // Newest to oldest, keep while they fit.
    let mut kept_rev: Vec<HistoryMessage> = Vec::new();
    let mut history_tokens = 0;
    let mut truncated = false;
    for msg in input.history.iter().rev() {
        let t = est(&msg.content);
        if t <= remaining {
            remaining -= t;
            history_tokens += t;
            kept_rev.push(msg.clone());
        } else {
            truncated = true;
            break;
        }
    }
    let mut kept: Vec<HistoryMessage> = kept_rev.into_iter().rev().collect();
    // Never start with an answer without its question.
    while kept.first().is_some_and(|m| m.role == "assistant") {
        let m = kept.remove(0);
        history_tokens -= est(&m.content);
    }

    Ok(ContextPlan {
        instructions: input.instructions,
        summary_message,
        summary_tokens,
        messages: kept,
        messages_truncated: truncated,
        assembled_tokens: mandatory + summary_tokens + history_tokens,
        effective_budget: limit,
    })
}

/// Thread summary trigger (DESIGN §3.6 "Summary trigger based on token budget").
#[must_use]
pub fn summary_trigger(plan: &ContextPlan, has_summary: bool, threshold_pct: u32) -> bool {
    if plan.messages_truncated {
        return true;
    }
    if has_summary {
        return false;
    }
    let threshold = plan.effective_budget.saturating_mul(i64::from(threshold_pct)) / 100;
    plan.assembled_tokens >= threshold
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod context_tests;
