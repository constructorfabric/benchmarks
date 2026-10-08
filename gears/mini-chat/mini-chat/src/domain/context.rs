//! Context plan assembly and truncation (DESIGN §4 "Context Plan Assembly and Truncation").

use mini_chat_sdk::ModelCatalogEntry;

use crate::domain::credits::{ToolFlags, estimate_text_tokens};
use crate::domain::error::DomainError;
use crate::infra::llm::{ContentPart, InputMessage, Role};

/// Preamble sent before the thread summary (DESIGN B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// A history message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    /// Role.
    pub role: Role,
    /// Text.
    pub content: String,
}

/// An image on the current message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// RAG provider file id.
    pub file_id: String,
    /// Anthropic copy.
    pub secondary_file_id: Option<String>,
}

/// Inputs of the assembly.
#[derive(Debug, Clone)]
pub struct ContextInputs<'a> {
    /// Effective model.
    pub model: &'a ModelCatalogEntry,
    /// Applied output cap.
    pub max_output_tokens_applied: u32,
    /// Tools sent.
    pub tools: ToolFlags,
    /// `search_knowledge` sent.
    pub knowledge_search: bool,
    /// Guards (file search, web search, knowledge search).
    pub guards: (&'a str, &'a str, &'a str),
    /// Thread summary text and stored token estimate.
    pub summary: Option<(String, i32)>,
    /// Recent messages, chronological.
    pub recent: Vec<HistoryMessage>,
    /// Current message text.
    pub current_text: &'a str,
    /// Current images.
    pub current_images: Vec<ImageRef>,
}

/// Assembled context.
#[derive(Debug, Clone)]
pub struct ContextPlan {
    /// System instructions (prompt + guards).
    pub instructions: String,
    /// Input messages.
    pub input: Vec<InputMessage>,
    /// Estimated tokens of the assembled context.
    pub assembled_tokens: i64,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
    /// At least one recent message was dropped for the budget.
    pub messages_truncated: bool,
    /// Token estimate of the applied summary.
    pub summary_applied: Option<i32>,
}

/// Effective input budget (no surcharges): `min(max_input, context_window - max_output)`.
#[must_use]
pub fn effective_budget(model: &ModelCatalogEntry, max_output_applied: u32) -> i64 {
    let window = if model.context_window > 0 {
        i64::from(model.context_window) - i64::from(max_output_applied)
    } else {
        i64::MAX
    };
    if model.max_input_tokens > 0 { window.min(i64::from(model.max_input_tokens)) } else { window }
}

/// Assembles the context plan.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items do not fit.
pub fn assemble(inp: &ContextInputs<'_>) -> Result<ContextPlan, DomainError> {
    let b = &inp.model.estimation_budgets;
    let input_limit = effective_budget(inp.model, inp.max_output_tokens_applied);
    if inp.model.context_window > 0 && i64::from(inp.max_output_tokens_applied) >= i64::from(inp.model.context_window) {
        return Err(DomainError::ContextBudgetExceeded);
    }
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
    let budget = input_limit.saturating_sub(deductions);
    if budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }

    let mut instructions = inp.model.system_prompt.clone();
    let mut push_guard = |g: &str| {
        if g.is_empty() {
            return;
        }
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(g);
    };
    if inp.tools.file_search {
        push_guard(inp.guards.0);
    }
    if inp.tools.web_search {
        push_guard(inp.guards.1);
    }
    if inp.knowledge_search {
        push_guard(inp.guards.2);
    }

    let est = |s: &str| estimate_text_tokens(s.len(), b);
    let image_tokens = i64::try_from(inp.current_images.len()).unwrap_or(0) * i64::from(b.image_token_budget);
    let mandatory = est(&instructions) + est(inp.current_text) + image_tokens;
    if mandatory > budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut used = mandatory;

    let mut summary_msg = None;
    let mut summary_applied = None;
    if let Some((text, token_estimate)) = &inp.summary {
        let full = format!("{SUMMARY_PREAMBLE}\n\n{text}");
        let cost = est(&full);
        if used + cost <= budget {
            used += cost;
            summary_msg = Some(InputMessage::text(Role::User, full));
            summary_applied = Some(*token_estimate);
        }
    }

    let mut kept_rev: Vec<&HistoryMessage> = Vec::new();
    let mut truncated = false;
    for m in inp.recent.iter().rev() {
        let cost = est(&m.content);
        if used + cost > budget {
            truncated = true;
            break;
        }
        used += cost;
        kept_rev.push(m);
    }
    let mut kept: Vec<&HistoryMessage> = kept_rev.into_iter().rev().collect();
    if truncated {
        while kept.first().is_some_and(|m| m.role == Role::Assistant) {
            used -= est(&kept[0].content);
            kept.remove(0);
        }
    }

    let mut input = Vec::new();
    if let Some(s) = summary_msg {
        input.push(s);
    }
    input.extend(kept.iter().map(|m| InputMessage::text(m.role, m.content.clone())));
    let mut parts = vec![ContentPart::Text(inp.current_text.to_owned())];
    for img in &inp.current_images {
        parts.push(ContentPart::Image { file_id: img.file_id.clone(), secondary_file_id: img.secondary_file_id.clone() });
    }
    input.push(InputMessage { role: Role::User, parts });

    Ok(ContextPlan {
        instructions,
        input,
        assembled_tokens: used,
        effective_budget: input_limit,
        messages_truncated: truncated,
        summary_applied,
    })
}

/// Thread-summary trigger decision (DESIGN "Trigger timing").
#[must_use]
pub fn summary_trigger(plan: &ContextPlan, has_summary: bool, threshold_pct: u32) -> bool {
    if plan.messages_truncated {
        return true;
    }
    if has_summary {
        return false;
    }
    let threshold = (i128::from(plan.effective_budget) * i128::from(threshold_pct)).checked_div(100).unwrap_or(0);
    i128::from(plan.assembled_tokens) >= threshold
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod context_tests;
