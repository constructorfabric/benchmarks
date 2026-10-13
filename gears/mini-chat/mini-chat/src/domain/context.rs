//! Context plan assembly and truncation (DESIGN section 4, "Context Plan
//! Assembly and Truncation" and "Truncation Algorithm"; thread summary trigger
//! in section 3.6).
//!
//! Pure functions: no database, no async. The output uses the LLM request
//! types ([`InputItem`], [`ContentPart`]) directly.
//!
//! Item estimation: every item (the instructions, the summary message, each
//! history message, the user message) is estimated on its own with
//! [`estimate_text_tokens`], so the formula's `fixed_overhead_tokens` and
//! safety margin apply once per item. Each current-turn image adds
//! `image_token_budget`. The plan's `assembled_tokens` is the sum of these
//! item estimates.

use mini_chat_sdk::EstimationBudgets;

use crate::domain::enums::MessageRole;
use crate::domain::error::DomainError;
use crate::domain::estimation::estimate_text_tokens;
use crate::domain::ports::{ContentPart, InputItem};

/// Preamble prepended to the thread summary (DESIGN B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// One persisted history message (chronological order in
/// [`ContextInput::history`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryItem {
    pub role: MessageRole,
    pub content: String,
}

/// Inputs of [`assemble_context`].
#[derive(Debug)]
pub struct ContextInput<'a> {
    pub system_prompt: &'a str,
    /// Tool guard texts appended after the system prompt.
    pub guards: Vec<&'a str>,
    pub summary: Option<&'a str>,
    /// Recent messages, oldest first. `system`-role messages are skipped.
    pub history: Vec<HistoryItem>,
    pub user_message: &'a str,
    /// Provider file ids of the current turn's images.
    pub image_file_ids: Vec<String>,
    pub budgets: &'a EstimationBudgets,
    pub token_budget: i64,
}

/// The assembled model input.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPlan {
    pub instructions: String,
    pub input: Vec<InputItem>,
    pub assembled_tokens: i64,
    /// At least one history message was dropped.
    pub messages_truncated: bool,
    /// Estimated size of the summary message when it was kept.
    pub summary_token_estimate: Option<i64>,
}

fn tokens(text: &str, b: &EstimationBudgets) -> i64 {
    estimate_text_tokens(text.len(), b)
}

fn message(role: &'static str, part: ContentPart) -> InputItem {
    InputItem::Message {
        role,
        content: vec![part],
    }
}

/// Assemble the context plan within `token_budget`.
///
/// Mandatory items (instructions, current user message, images) are never
/// truncated. The summary is kept only if it fits after them; history is then
/// walked newest to oldest and kept while it fits (the first misfit and all
/// older messages are dropped), and a kept range never starts with an
/// assistant message.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items alone exceed the budget.
pub fn assemble_context(input: ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let b = input.budgets;
    let instructions = std::iter::once(input.system_prompt)
        .chain(input.guards.iter().copied())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let image_tokens = i64::from(b.image_token_budget)
        .saturating_mul(i64::try_from(input.image_file_ids.len()).unwrap_or(i64::MAX));
    let mandatory = tokens(&instructions, b)
        .saturating_add(tokens(input.user_message, b))
        .saturating_add(image_tokens);
    if mandatory > input.token_budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut used = mandatory;

    let mut summary_item = None;
    let mut summary_token_estimate = None;
    if let Some(summary) = input.summary {
        let text = format!("{SUMMARY_PREAMBLE}\n\n{summary}");
        let est = tokens(&text, b);
        if used.saturating_add(est) <= input.token_budget {
            used = used.saturating_add(est);
            summary_token_estimate = Some(est);
            summary_item = Some(message("user", ContentPart::InputText(text)));
        }
    }

    let history: Vec<(HistoryItem, i64)> = input
        .history
        .into_iter()
        .filter(|h| h.role != MessageRole::System)
        .map(|h| {
            let est = tokens(&h.content, b);
            (h, est)
        })
        .collect();
    let total = history.len();

    // Newest to oldest; the first misfit ends the walk.
    let mut start = total;
    for (_, est) in history.iter().rev() {
        let next = used.saturating_add(*est);
        if next > input.token_budget {
            break;
        }
        used = next;
        start -= 1;
    }
    // Never start with an assistant message: drop whole turns.
    while start < total && history[start].0.role == MessageRole::Assistant {
        used = used.saturating_sub(history[start].1);
        start += 1;
    }
    let messages_truncated = start > 0;

    let mut items = Vec::with_capacity(total - start + 2);
    items.extend(summary_item);
    for (h, _) in history.into_iter().skip(start) {
        items.push(match h.role {
            MessageRole::Assistant => message("assistant", ContentPart::OutputText(h.content)),
            _ => message("user", ContentPart::InputText(h.content)),
        });
    }
    let mut content = vec![ContentPart::InputText(input.user_message.to_owned())];
    content.extend(
        input
            .image_file_ids
            .into_iter()
            .map(|file_id| ContentPart::InputImage { file_id }),
    );
    items.push(InputItem::Message {
        role: "user",
        content,
    });

    Ok(ContextPlan {
        instructions,
        input: items,
        assembled_tokens: used,
        messages_truncated,
        summary_token_estimate,
    })
}

/// DESIGN section 3.6: fire when enabled and either the context was truncated
/// (urgent, re-summarize) or no summary exists yet and
/// `assembled >= effective_budget * threshold_pct / 100` (proactive).
#[must_use]
pub fn summary_trigger(
    enabled: bool,
    has_summary: bool,
    messages_truncated: bool,
    assembled: i64,
    effective_budget: i64,
    threshold_pct: u32,
) -> bool {
    if !enabled {
        return false;
    }
    if messages_truncated {
        return true;
    }
    if has_summary || effective_budget <= 0 {
        return false;
    }
    // `assembled >= budget * pct / 100`, cross-multiplied to avoid rounding.
    assembled.saturating_mul(100) >= effective_budget.saturating_mul(i64::from(threshold_pct))
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod context_tests;
