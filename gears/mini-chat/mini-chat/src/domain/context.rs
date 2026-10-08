//! `ContextPlan` assembly and deterministic truncation (DESIGN §4 "Context Plan Assembly and Truncation").

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};

use crate::domain::credits::estimate_text_tokens;
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::llm::responses::{InputMessage, Role};

/// Preamble prepended to the thread summary (DESIGN B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// One history message (chronological order).
#[derive(Debug, Clone)]
pub struct HistoryMessage {
    pub role: Role,
    pub text: String,
}

/// Inputs of context assembly.
pub struct ContextInputs<'a> {
    pub model: &'a ModelCatalogEntry,
    pub guards: Vec<String>,
    pub summary: Option<String>,
    pub history: Vec<HistoryMessage>,
    pub user_text: &'a str,
    pub image_file_ids: Vec<String>,
    pub max_output_tokens_applied: i64,
    pub surcharges: i64,
}

/// Assembled context of a turn.
#[derive(Debug, Clone)]
pub struct ContextPlan {
    pub instructions: String,
    pub messages: Vec<InputMessage>,
    /// Estimated tokens of the summary item when it was kept.
    pub summary_tokens: Option<i64>,
    pub assembled_tokens: i64,
    pub messages_truncated: bool,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
}

/// Input limit of a model: `min(max_input_tokens, context_window - mota)` (`0` = no separate limit).
#[must_use]
pub fn input_limit(model: &ModelCatalogEntry, mota: i64) -> i64 {
    let window = i64::from(model.context_window) - mota;
    if model.max_input_tokens > 0 {
        window.min(i64::from(model.max_input_tokens))
    } else {
        window
    }
}

/// System instructions: the model's system prompt plus the tool guards.
#[must_use]
pub fn instructions(model: &ModelCatalogEntry, guards: &[String]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !model.system_prompt.trim().is_empty() {
        parts.push(model.system_prompt.trim());
    }
    for g in guards {
        if !g.trim().is_empty() {
            parts.push(g.trim());
        }
    }
    parts.join("\n\n")
}

fn est(text: &str, b: &EstimationBudgets) -> i64 {
    estimate_text_tokens(text, b)
}

/// Assembles and truncates the context.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items do not fit.
pub fn assemble(inp: ContextInputs<'_>) -> DomainResult<ContextPlan> {
    let b = &inp.model.estimation_budgets;
    if inp.max_output_tokens_applied >= i64::from(inp.model.context_window) {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let limit = input_limit(inp.model, inp.max_output_tokens_applied);
    let budget = limit - inp.surcharges - i64::from(b.fixed_overhead_tokens);
    if budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let instructions = instructions(inp.model, &inp.guards);
    #[allow(clippy::integer_division)] // exact constant: overflow-safe saturation cap
    let images = i64::try_from(inp.image_file_ids.len()).unwrap_or(i64::MAX / 2);
    let mandatory =
        est(&instructions, b) + est(inp.user_text, b) + images * i64::from(b.image_token_budget);
    if mandatory > budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut remaining = budget - mandatory;
    let mut assembled = mandatory;

    let mut summary_item = None;
    let mut summary_tokens = None;
    if let Some(s) = inp.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        let text = format!("{SUMMARY_PREAMBLE}\n\n{s}");
        let t = est(&text, b);
        if t <= remaining {
            remaining -= t;
            assembled += t;
            summary_tokens = Some(t);
            summary_item = Some(InputMessage {
                role: Role::User,
                text,
                image_file_ids: Vec::new(),
            });
        }
    }

    // Recent messages: newest to oldest while they fit.
    let total = inp.history.len();
    let mut kept_rev: Vec<&HistoryMessage> = Vec::new();
    for m in inp.history.iter().rev() {
        let t = est(&m.text, b);
        if t > remaining {
            break;
        }
        remaining -= t;
        assembled += t;
        kept_rev.push(m);
    }
    let mut kept: Vec<&HistoryMessage> = kept_rev.into_iter().rev().collect();
    // Never send an answer without its question.
    while kept.first().is_some_and(|m| m.role == Role::Assistant) && kept.len() < total {
        let m = kept.remove(0);
        assembled -= est(&m.text, b);
    }
    let messages_truncated = kept.len() < total;

    let mut messages = Vec::new();
    if let Some(s) = summary_item {
        messages.push(s);
    }
    for m in kept {
        messages.push(InputMessage {
            role: m.role,
            text: m.text.clone(),
            image_file_ids: Vec::new(),
        });
    }
    messages.push(InputMessage {
        role: Role::User,
        text: inp.user_text.to_owned(),
        image_file_ids: inp.image_file_ids,
    });
    Ok(ContextPlan {
        instructions,
        messages,
        summary_tokens,
        assembled_tokens: assembled,
        messages_truncated,
        effective_budget: limit,
    })
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod context_tests;
