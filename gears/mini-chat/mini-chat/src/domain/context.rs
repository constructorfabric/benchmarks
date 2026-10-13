//! Context plan assembly and truncation (S§8, D "Context Plan Assembly and
//! Truncation").

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::estimation::estimate_text_tokens;
use crate::infra::llm::types::{ContentPart, InputMessage, Role};

/// Separator between instruction parts and between preamble and summary.
const PART_SEPARATOR: &str = "\n\n";

/// Prepended to the thread summary (D B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// Thread summary as stored (`token_estimate` is reported back in
/// [`ContextPlan::summary_applied`]; the budget uses the size of the text
/// actually sent, preamble included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryForContext {
    pub text: String,
    pub token_estimate: i64,
}

/// A persisted message of the recent-history window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub id: Uuid,
    pub role: Role,
    pub content: String,
    pub created_at: OffsetDateTime,
}

/// Everything context assembly depends on.
#[derive(Debug, Clone)]
pub struct ContextInput<'a> {
    pub eff: &'a ModelCatalogEntry,
    pub max_output_tokens_applied: u32,
    pub system_prompt: &'a str,
    pub guards: Vec<&'a str>,
    pub summary: Option<SummaryForContext>,
    pub recent: Vec<HistoryMessage>,
    pub current_text: &'a str,
    pub current_images: Vec<String>,
    pub surcharges: i64,
}

/// Result of [`assemble`].
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPlan {
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub messages_truncated: bool,
    pub assembled_context_tokens: i64,
    pub summary_applied: Option<i64>,
    pub effective_budget: i64,
}

/// Assembles the provider input for the current turn: mandatory items, then
/// the thread summary if it fits, then the newest recent messages that fit.
///
/// # Errors
/// `ContextBudgetExceeded` when the budget is invalid or the mandatory items
/// do not fit.
pub fn assemble(input: ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let eff = input.eff;
    let budgets = &eff.estimation_budgets;
    let tokens = |bytes: usize| estimate_text_tokens(bytes, budgets);

    let effective_budget = input_limit(eff, input.max_output_tokens_applied)?;
    let token_budget = effective_budget
        .saturating_sub(input.surcharges)
        .saturating_sub(i64::from(budgets.fixed_overhead_tokens));
    if token_budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }

    let instructions = std::iter::once(input.system_prompt)
        .chain(input.guards.iter().copied())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(PART_SEPARATOR);

    // 1. Mandatory items.
    let image_tokens = i64::try_from(input.current_images.len())
        .unwrap_or(i64::MAX)
        .saturating_mul(i64::from(budgets.image_token_budget));
    let mandatory = tokens(instructions.len())
        .saturating_add(tokens(input.current_text.len()))
        .saturating_add(image_tokens);
    if mandatory > token_budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut used = mandatory;

    // 2. Thread summary: kept whole or not at all.
    let summary = input.summary.and_then(|s| {
        let text = format!("{SUMMARY_PREAMBLE}{PART_SEPARATOR}{}", s.text);
        let size = tokens(text.len());
        (size <= token_budget - used).then_some((text, size, s.token_estimate))
    });
    if let Some((_, size, _)) = &summary {
        used = used.saturating_add(*size);
    }

    // 3. Recent messages: newest first while they fit, whole turns only.
    let mut recent = input.recent;
    recent.sort_by_key(|m| (m.created_at, m.id));
    let costs: Vec<i64> = recent.iter().map(|m| tokens(m.content.len())).collect();
    let mut first_kept = recent.len();
    for (i, cost) in costs.iter().enumerate().rev() {
        if *cost > token_budget - used {
            break;
        }
        used += cost;
        first_kept = i;
    }
    while first_kept < recent.len() && recent[first_kept].role == Role::Assistant {
        used -= costs[first_kept];
        first_kept += 1;
    }

    let mut messages = Vec::with_capacity(recent.len() - first_kept + 2);
    let summary_applied = summary.as_ref().map(|(_, _, estimate)| *estimate);
    if let Some((text, _, _)) = summary {
        messages.push(InputMessage::text(Role::User, text));
    }
    messages.extend(
        recent
            .drain(first_kept..)
            .map(|m| InputMessage::text(m.role, m.content)),
    );
    let mut current = vec![ContentPart::Text(input.current_text.to_owned())];
    current.extend(
        input
            .current_images
            .into_iter()
            .map(|file_id| ContentPart::Image { file_id }),
    );
    messages.push(InputMessage {
        role: Role::User,
        content: current,
    });

    Ok(ContextPlan {
        instructions,
        input: messages,
        messages_truncated: first_kept > 0,
        assembled_context_tokens: used,
        summary_applied,
        effective_budget,
    })
}

/// `min(max_input_tokens (0 = none), context_window - max_out)`.
fn input_limit(eff: &ModelCatalogEntry, max_out: u32) -> Result<i64, DomainError> {
    if max_out >= eff.context_window {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let window_limit = i64::from(eff.context_window) - i64::from(max_out);
    Ok(match eff.max_input_tokens {
        0 => window_limit,
        max_in => window_limit.min(i64::from(max_in)),
    })
}

/// The current message alone exceeds `max_input_tokens` (`0` = no limit).
#[must_use]
pub fn input_too_long(eff: &ModelCatalogEntry, current_text: &str) -> bool {
    eff.max_input_tokens != 0
        && estimate_text_tokens(current_text.len(), &eff.estimation_budgets)
            > i64::from(eff.max_input_tokens)
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
