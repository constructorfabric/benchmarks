//! Context plan assembly and truncation (DESIGN "Context Plan Assembly and
//! Truncation"). Deterministic for identical inputs.

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};

use super::error::DomainError;
use super::estimation::estimate_item_tokens;

/// Preamble prepended to the thread summary (B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// Role of a history message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRole {
    User,
    Assistant,
}

/// One history message (chronological order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub role: HistoryRole,
    pub content: String,
}

/// Inputs of the assembly.
#[derive(Debug, Clone)]
pub struct ContextInput<'a> {
    pub model: &'a ModelCatalogEntry,
    pub max_output_tokens_applied: i64,
    /// System prompt plus tool guard instructions.
    pub instructions: String,
    pub summary: Option<String>,
    /// Recent messages, chronological.
    pub history: Vec<HistoryMessage>,
    pub user_text: &'a str,
    pub image_count: u32,
    /// Sum of the tool surcharges of the request.
    pub surcharges: i64,
}

/// Assembled context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    pub instructions: String,
    /// Summary message text (preamble + summary) when kept.
    pub summary_message: Option<String>,
    pub history: Vec<HistoryMessage>,
    pub messages_truncated: bool,
    pub assembled_tokens: i64,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
}

/// Effective input budget of a model (0 `max_input_tokens` = no separate limit).
#[must_use]
pub fn effective_budget(model: &ModelCatalogEntry, max_output_tokens_applied: i64) -> i64 {
    let window = i64::from(model.context_window) - max_output_tokens_applied;
    if model.max_input_tokens > 0 {
        window.min(i64::from(model.max_input_tokens))
    } else {
        window
    }
}

fn tokens(s: &str, b: &EstimationBudgets) -> i64 {
    estimate_item_tokens(s.len(), b)
}

/// Builds the summary message text.
#[must_use]
pub fn summary_message(summary: &str) -> String {
    format!("{SUMMARY_PREAMBLE}\n\n{summary}")
}

/// Assembles the context plan.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items do not fit.
// False positive: images count times per-image budget is intended.
#[allow(clippy::suspicious_operation_groupings)]
pub fn assemble(input: ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let b = &input.model.estimation_budgets;
    let effective = effective_budget(input.model, input.max_output_tokens_applied);
    if input.max_output_tokens_applied >= i64::from(input.model.context_window) || effective <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let token_budget = effective - input.surcharges - i64::from(b.fixed_overhead_tokens);
    if token_budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mandatory = tokens(&input.instructions, b)
        + tokens(input.user_text, b)
        + i64::from(input.image_count) * i64::from(b.image_token_budget);
    if mandatory > token_budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut used = mandatory;
    let mut summary_message_text = input.summary.as_deref().map(summary_message);
    if let Some(s) = &summary_message_text {
        let t = tokens(s, b);
        if used + t <= token_budget {
            used += t;
        } else {
            summary_message_text = None;
        }
    }
    // Newest to oldest while they fit.
    let mut kept_rev: Vec<HistoryMessage> = Vec::new();
    let mut truncated = false;
    for m in input.history.iter().rev() {
        let t = tokens(&m.content, b);
        if used + t <= token_budget {
            used += t;
            kept_rev.push(m.clone());
        } else {
            truncated = true;
            break;
        }
    }
    let mut kept: Vec<HistoryMessage> = kept_rev.into_iter().rev().collect();
    // Never send an answer without its question.
    while kept.first().is_some_and(|m| m.role == HistoryRole::Assistant) && truncated {
        let m = kept.remove(0);
        used -= tokens(&m.content, b);
    }
    Ok(ContextPlan {
        instructions: input.instructions,
        summary_message: summary_message_text,
        history: kept,
        messages_truncated: truncated,
        assembled_tokens: used,
        effective_budget: effective,
    })
}

/// Whether the thread summary trigger fires (DESIGN "Summary trigger").
#[must_use]
#[allow(clippy::integer_division)] // floored percentage threshold is intended
pub fn summary_trigger(plan: &ContextPlan, has_summary: bool, threshold_pct: u32) -> bool {
    if plan.messages_truncated {
        return true;
    }
    if has_summary {
        return false;
    }
    let threshold = plan.effective_budget * i64::from(threshold_pct) / 100;
    plan.assembled_tokens >= threshold
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod context_tests;
