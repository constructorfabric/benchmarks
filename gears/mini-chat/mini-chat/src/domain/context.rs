//! Context plan assembly and truncation (DESIGN sections 2.2 "Context Window Budget" and 3.4
//! "Context Plan Assembly and Truncation").

use mini_chat_sdk::ModelCatalogEntry;
use toolkit_macros::domain_model;

use super::error::DomainError;
use super::estimation::{ToolSet, estimated_text_tokens, tool_surcharge_tokens};
use super::llm::{InputMessage, InputRole};

/// Preamble prepended to the thread summary (DESIGN B.5.5).
pub const THREAD_SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been \
summarized. The summary below covers the earlier portion of the conversation. Recent messages \
follow after.";

/// `INPUT_TOO_LONG`: the current message alone exceeds the model's `max_input_tokens`
/// (`0` disables the check).
///
/// # Errors
/// `InputTooLong`.
pub fn check_input_length(
    entry: &ModelCatalogEntry,
    content_bytes: u64,
) -> Result<(), DomainError> {
    if entry.max_input_tokens == 0 {
        return Ok(());
    }
    let tokens = estimated_text_tokens(content_bytes, &entry.estimation_budgets);
    if tokens > i64::from(entry.max_input_tokens) {
        Err(DomainError::InputTooLong)
    } else {
        Ok(())
    }
}

/// `min(max_input_tokens, context_window - max_output_tokens_applied)` of a model
/// (`max_input_tokens = 0`: no separate limit). `None` when the output cap fills the window.
#[must_use]
pub fn effective_input_limit(
    entry: &ModelCatalogEntry,
    max_output_tokens_applied: u32,
) -> Option<i64> {
    let window = i64::from(entry.context_window);
    let room = window - i64::from(max_output_tokens_applied);
    if room <= 0 {
        return None;
    }
    if entry.max_input_tokens == 0 {
        Some(room)
    } else {
        Some(room.min(i64::from(entry.max_input_tokens)))
    }
}

/// What goes into the context besides the model entry.
#[domain_model]
#[derive(Debug, Clone)]
pub struct ContextInputs<'a> {
    /// System prompt including the tool guards.
    pub system_prompt: &'a str,
    /// The built-in tools of the request.
    pub tools: ToolSet,
    /// `min(catalog max_output_tokens, streaming cap)`.
    pub max_output_tokens_applied: u32,
    /// The current user message.
    pub current_text: &'a str,
    /// Images attached to the current message.
    pub image_count: u32,
    /// The thread summary text, when a summary row exists.
    pub summary: Option<&'a str>,
    /// Recent messages in chronological order, not covered by the summary.
    pub history: Vec<InputMessage>,
}

/// The assembled context.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    /// The summary message (preamble and summary text) when the summary was kept.
    pub summary_message: Option<String>,
    /// The estimated tokens of the kept summary message.
    pub summary_tokens: Option<i64>,
    /// The kept recent messages in chronological order.
    pub history: Vec<InputMessage>,
    /// At least one recent message was dropped.
    pub messages_truncated: bool,
    /// Estimated tokens of the whole assembled context (system prompt, summary, history, current).
    pub assembled_context_tokens: i64,
}

fn text_tokens(entry: &ModelCatalogEntry, text: &str) -> i64 {
    estimated_text_tokens(text.len() as u64, &entry.estimation_budgets)
}

/// Assembles the context for the effective model: mandatory items first, then the thread summary
/// when it fits, then recent messages newest first (oldest whole turns are dropped first).
///
/// # Errors
/// `ContextBudgetExceeded` when the output cap fills the window, the deductions reach the input
/// limit or the mandatory items do not fit the token budget.
pub fn assemble(
    entry: &ModelCatalogEntry,
    inputs: ContextInputs<'_>,
) -> Result<ContextPlan, DomainError> {
    let limit = effective_input_limit(entry, inputs.max_output_tokens_applied)
        .ok_or(DomainError::ContextBudgetExceeded)?;
    let deductions = tool_surcharge_tokens(&entry.estimation_budgets, inputs.tools)
        .saturating_add(i64::from(entry.estimation_budgets.fixed_overhead_tokens));
    if deductions >= limit {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let budget = limit - deductions;

    let image_tokens = i64::from(inputs.image_count)
        .saturating_mul(i64::from(entry.estimation_budgets.image_token_budget));
    let mandatory = text_tokens(entry, inputs.system_prompt)
        .saturating_add(text_tokens(entry, inputs.current_text))
        .saturating_add(image_tokens);
    if mandatory > budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut used = mandatory;

    let (summary_message, summary_tokens) = match inputs.summary {
        Some(summary) => {
            let message = format!("{THREAD_SUMMARY_PREAMBLE}\n\n{summary}");
            let tokens = text_tokens(entry, &message);
            if used.saturating_add(tokens) <= budget {
                used += tokens;
                (Some(message), Some(tokens))
            } else {
                (None, None)
            }
        }
        None => (None, None),
    };

    let mut kept_rev: Vec<InputMessage> = Vec::new();
    let mut truncated = false;
    let mut remaining = inputs.history;
    while let Some(message) = remaining.pop() {
        let tokens = text_tokens(entry, &message.text);
        if used.saturating_add(tokens) > budget {
            truncated = true;
            break;
        }
        used += tokens;
        kept_rev.push(message);
    }
    truncated |= !remaining.is_empty();
    let mut history: Vec<InputMessage> = kept_rev.into_iter().rev().collect();
    // An answer is never sent without its question: whole turns are dropped.
    while history
        .first()
        .is_some_and(|message| message.role == InputRole::Assistant)
    {
        let dropped = history.remove(0);
        used -= text_tokens(entry, &dropped.text);
        truncated = true;
    }

    Ok(ContextPlan {
        summary_message,
        summary_tokens,
        history,
        messages_truncated: truncated,
        assembled_context_tokens: used,
    })
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
