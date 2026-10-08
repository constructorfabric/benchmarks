//! `ContextPlan` assembly and truncation (DESIGN §4 "Context Plan Assembly and Truncation").

use mini_chat_sdk::ModelCatalogEntry;

use crate::domain::error::{DomainError, Resource};
use crate::domain::quota_math::estimate_item_tokens;
use crate::domain::service::quota::ToolPlan;
use crate::infra::llm::responses::{InputMessage, InputRole};

/// Preamble prepended to the thread summary (DESIGN B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// One history message (chronological order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub role: InputRole,
    pub content: String,
}

/// Inputs of the assembly.
#[derive(Debug, Clone)]
pub struct ContextInputs<'a> {
    pub model: &'a ModelCatalogEntry,
    pub max_output_tokens_applied: u32,
    pub tools: ToolPlan,
    pub web_search_guard: &'a str,
    pub file_search_guard: &'a str,
    pub summary: Option<&'a str>,
    pub history: Vec<HistoryMessage>,
    pub user_text: &'a str,
    pub image_file_ids: Vec<String>,
}

/// The assembled request context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub summary_included: bool,
    pub assembled_tokens: i64,
    pub messages_truncated: bool,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)` (no surcharges).
    pub effective_budget: i64,
}

fn budget_exceeded() -> DomainError {
    DomainError::out_of_range(
        Resource::Chat,
        "content",
        "CONTEXT_BUDGET_EXCEEDED",
        "mandatory context does not fit the model's input budget",
    )
}

/// Input limit `min(max_input_tokens, context_window - max_output_tokens_applied)` (`0` = no separate limit).
#[must_use]
pub fn input_limit(model: &ModelCatalogEntry, max_output_applied: u32) -> i64 {
    let window = i64::from(model.context_window) - i64::from(max_output_applied);
    if model.max_input_tokens > 0 {
        window.min(i64::from(model.max_input_tokens))
    } else {
        window
    }
}

/// Builds the system instructions (catalog prompt + guards of the tools sent).
#[must_use]
pub fn build_instructions(
    model: &ModelCatalogEntry,
    tools: ToolPlan,
    web_guard: &str,
    file_guard: &str,
) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !model.system_prompt.trim().is_empty() {
        parts.push(model.system_prompt.as_str());
    }
    if tools.file_search && !file_guard.is_empty() {
        parts.push(file_guard);
    }
    if tools.web_search && !web_guard.is_empty() {
        parts.push(web_guard);
    }
    parts.join("\n\n")
}

/// Assembles the context within the budget.
///
/// # Errors
/// 400 `CONTEXT_BUDGET_EXCEEDED` when the mandatory items do not fit.
pub fn assemble(inp: ContextInputs<'_>) -> Result<ContextPlan, DomainError> {
    let m = inp.model;
    let b = &m.estimation_budgets;
    if inp.max_output_tokens_applied >= m.context_window {
        return Err(budget_exceeded());
    }
    let limit = input_limit(m, inp.max_output_tokens_applied);
    let mut deductions = i64::from(b.fixed_overhead_tokens);
    if inp.tools.file_search {
        deductions += i64::from(b.tool_surcharge_tokens);
    }
    if inp.tools.web_search {
        deductions += i64::from(b.web_search_surcharge_tokens);
    }
    if inp.tools.code_interpreter {
        deductions += i64::from(b.code_interpreter_surcharge_tokens);
    }
    if deductions >= limit {
        return Err(budget_exceeded());
    }
    let budget = limit - deductions;
    let est = |s: &str| estimate_item_tokens(s.len(), b);

    let instructions =
        build_instructions(m, inp.tools, inp.web_search_guard, inp.file_search_guard);
    let images = i64::try_from(inp.image_file_ids.len()).unwrap_or(i64::MAX);
    let mandatory = est(&instructions)
        + est(inp.user_text)
        + images.saturating_mul(i64::from(b.image_token_budget));
    if mandatory > budget {
        return Err(budget_exceeded());
    }
    let mut remaining = budget - mandatory;
    let mut assembled = mandatory;

    let summary_text = inp.summary.map(|s| format!("{SUMMARY_PREAMBLE}\n\n{s}"));
    let mut summary_included = false;
    if let Some(s) = &summary_text {
        let t = est(s);
        if t <= remaining {
            remaining -= t;
            assembled += t;
            summary_included = true;
        }
    }

    // Newest to oldest; the first message that does not fit and all older ones are dropped.
    let mut kept: Vec<&HistoryMessage> = Vec::new();
    let mut truncated = false;
    for (i, h) in inp.history.iter().enumerate().rev() {
        let t = est(&h.content);
        if t <= remaining {
            remaining -= t;
            assembled += t;
            kept.push(h);
        } else {
            truncated = i < inp.history.len();
            break;
        }
    }
    kept.reverse();
    // Never send an answer without its question.
    while kept.first().is_some_and(|h| h.role == InputRole::Assistant) {
        let h = kept.remove(0);
        assembled -= est(&h.content);
        truncated = true;
    }

    let mut input = Vec::with_capacity(kept.len() + 2);
    if summary_included && let Some(s) = summary_text {
        input.push(InputMessage {
            role: InputRole::User,
            text: s,
            image_file_ids: Vec::new(),
        });
    }
    for h in kept {
        input.push(InputMessage {
            role: h.role,
            text: h.content.clone(),
            image_file_ids: Vec::new(),
        });
    }
    input.push(InputMessage {
        role: InputRole::User,
        text: inp.user_text.to_owned(),
        image_file_ids: inp.image_file_ids,
    });
    Ok(ContextPlan {
        instructions,
        input,
        summary_included,
        assembled_tokens: assembled,
        messages_truncated: truncated,
        effective_budget: limit,
    })
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
