//! Context plan assembly and truncation (DESIGN §4 "Context Plan Assembly and
//! Truncation", "Context Plan Truncation Algorithm", §2.2 "Context Window Budget").
//!
//! [`assemble`] is pure: the caller loads the thread summary and the recent
//! messages (`MessageRepo::recent_for_context`, newest first, to be reversed) and
//! passes them in; the same input always yields the same [`ContextPlan`].

use mini_chat_sdk::ModelCatalogEntry;

use crate::config::MiniChatConfig;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::estimation::item_tokens;
use crate::domain::model::MessageRole;
use crate::infra::llm::types::LlmMessage;

/// Preamble of the thread summary message (DESIGN B.5.5), followed by `\n\n` and
/// the summary text.
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// Tools sent with a turn (decides the guard instructions).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSet {
    /// Vector store id when `file_search` is sent.
    pub file_search: Option<String>,
    pub web_search: bool,
    /// Provider file ids of the `code_interpreter` container (empty = tool not sent).
    pub code_interpreter: Vec<String>,
    pub search_knowledge: bool,
}

/// Guard instructions for the tools in `tools`, in the order `file_search`,
/// `web_search`, `search_knowledge` (each only when that tool is sent).
#[must_use]
pub fn guards_for(tools: &ToolSet, cfg: &MiniChatConfig) -> Vec<String> {
    let mut guards = Vec::new();
    if tools.file_search.is_some() {
        guards.push(cfg.context.file_search_guard.clone());
    }
    if tools.web_search {
        guards.push(cfg.context.web_search_guard.clone());
    }
    if tools.search_knowledge {
        guards.push(cfg.knowledge_search.guard.clone());
    }
    guards
}

/// One earlier message of the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    /// `user` or `assistant`.
    pub role: MessageRole,
    pub content: String,
}

/// Everything [`assemble`] needs.
#[derive(Debug, Clone)]
pub struct ContextInput {
    /// Catalog entry of the effective model.
    pub entry: ModelCatalogEntry,
    pub max_output_tokens_applied: i64,
    /// Tool guard instructions ([`guards_for`]), appended to the system prompt.
    pub system_prompt_extra: Vec<String>,
    /// Thread summary text and its stored token estimate.
    pub summary: Option<(String, i64)>,
    /// Recent messages, chronological, already filtered (live, not compressed,
    /// after the summary frontier, within the snapshot boundary).
    pub history: Vec<HistoryMessage>,
    pub user_text: String,
    /// Provider file ids of the images of the current message.
    pub image_file_ids: Vec<String>,
    /// Sum of the surcharges of the tools sent.
    pub surcharges: i64,
}

/// The assembled provider input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    /// System prompt plus guards, joined by a blank line.
    pub instructions: String,
    /// Summary (user role), kept history, then the current user message with its images.
    pub messages: Vec<LlmMessage>,
    /// Estimated tokens of the summary message (preamble included) when it was kept.
    pub summary_applied: Option<i64>,
    /// At least one recent message was dropped to fit the budget.
    pub messages_truncated: bool,
    /// Sum of the estimates of everything kept (instructions, summary, history,
    /// user message, images); drives the thread-summary trigger.
    pub assembled_context_tokens: i64,
    /// `min(max_input_tokens (if > 0), context_window - max_output_tokens_applied)`;
    /// no surcharges or fixed overhead deducted.
    pub effective_budget: i64,
}

/// Build the context plan, dropping the thread summary and the oldest recent
/// messages as needed to fit the token budget.
///
/// # Errors
/// [`DomainError::ContextBudgetExceeded`] when the output reservation reaches the
/// context window, the deductions consume the whole input limit, or the mandatory
/// items (system instructions, user message, images) alone exceed the budget.
pub fn assemble(input: &ContextInput) -> DomainResult<ContextPlan> {
    let entry = &input.entry;
    let b = &entry.estimation_budgets;

    let context_window = i64::from(entry.context_window);
    if input.max_output_tokens_applied >= context_window {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut effective_budget = context_window - input.max_output_tokens_applied.max(0);
    if entry.max_input_tokens > 0 {
        effective_budget = effective_budget.min(i64::from(entry.max_input_tokens));
    }
    let token_budget = effective_budget
        .saturating_sub(input.surcharges.max(0))
        .saturating_sub(i64::from(b.fixed_overhead_tokens));
    if token_budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }

    // 1. Mandatory items.
    let instructions = std::iter::once(entry.system_prompt.as_str())
        .chain(input.system_prompt_extra.iter().map(String::as_str))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let image_count = i64::try_from(input.image_file_ids.len()).unwrap_or(i64::MAX);
    let mandatory = item_tokens(&instructions, b)
        .saturating_add(item_tokens(&input.user_text, b))
        .saturating_add(image_count.saturating_mul(i64::from(b.image_token_budget)));
    if mandatory > token_budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut remaining = token_budget - mandatory;
    let mut assembled = mandatory;

    // 2. Thread summary: kept whole or dropped.
    let mut summary_message = None;
    let mut summary_applied = None;
    if let Some((text, stored_estimate)) = &input.summary {
        let message = format!("{SUMMARY_PREAMBLE}\n\n{text}");
        let tokens = item_tokens(&message, b).max(*stored_estimate);
        if tokens <= remaining {
            remaining -= tokens;
            assembled = assembled.saturating_add(tokens);
            summary_applied = Some(tokens);
            summary_message = Some(LlmMessage::text(MessageRole::User, message));
        }
    }

    // 3. Recent messages, newest to oldest, stopping at the first that does not fit.
    let mut keep_from = input.history.len();
    let mut kept_tokens = Vec::new();
    for (idx, msg) in input.history.iter().enumerate().rev() {
        let tokens = item_tokens(&msg.content, b);
        if tokens > remaining {
            break;
        }
        remaining -= tokens;
        kept_tokens.push(tokens);
        keep_from = idx;
    }
    // Kept tokens are listed newest first; an answer is never sent without its question.
    while keep_from < input.history.len() && input.history[keep_from].role != MessageRole::User {
        kept_tokens.pop();
        keep_from += 1;
    }
    let messages_truncated = keep_from > 0;
    assembled = assembled.saturating_add(kept_tokens.iter().sum::<i64>());

    let mut messages = Vec::with_capacity(input.history.len() - keep_from + 2);
    messages.extend(summary_message);
    messages.extend(
        input.history[keep_from..]
            .iter()
            .map(|m| LlmMessage::text(m.role, m.content.clone())),
    );
    messages.push(LlmMessage {
        role: MessageRole::User,
        text: input.user_text.clone(),
        image_file_ids: input.image_file_ids.clone(),
    });

    Ok(ContextPlan {
        instructions,
        messages,
        summary_applied,
        messages_truncated,
        assembled_context_tokens: assembled,
        effective_budget,
    })
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod context_tests;
