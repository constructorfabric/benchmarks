//! Context plan assembly and deterministic truncation (DESIGN §4).

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};

use super::quota::{ToolSet, estimate_text_tokens};

/// Preamble prepended to the thread summary (B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// A history message for context assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

/// Inputs of context assembly.
#[derive(Debug, Clone)]
pub struct ContextInputs<'a> {
    pub model: &'a ModelCatalogEntry,
    pub max_output_tokens_applied: i64,
    pub tools: ToolSet,
    pub system_instructions: String,
    pub summary: Option<String>,
    pub recent: Vec<HistoryMessage>,
    pub user_message: &'a str,
    pub image_count: usize,
}

/// The assembled context plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    pub instructions: String,
    /// Summary text with the preamble, when kept.
    pub summary: Option<String>,
    pub summary_token_estimate: Option<i64>,
    pub recent: Vec<HistoryMessage>,
    pub messages_truncated: bool,
    pub assembled_context_tokens: i64,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
}

/// `CONTEXT_BUDGET_EXCEEDED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("mandatory context does not fit the budget")]
pub struct ContextBudgetExceeded;

/// Effective input budget of a model (without surcharges / overhead).
pub fn input_limit(m: &ModelCatalogEntry, max_out: i64) -> Result<i64, ContextBudgetExceeded> {
    let ctx = i64::from(m.context_window);
    let max_in = i64::from(m.max_input_tokens);
    let window_limit = if ctx == 0 {
        i64::MAX
    } else {
        if max_out >= ctx {
            return Err(ContextBudgetExceeded);
        }
        ctx - max_out
    };
    Ok(if max_in > 0 {
        max_in.min(window_limit)
    } else {
        window_limit
    })
}

fn est(text: &str, b: &EstimationBudgets) -> i64 {
    estimate_text_tokens(text.len(), b)
}

/// Assemble and truncate the context plan.
pub fn assemble(inp: ContextInputs<'_>) -> Result<ContextPlan, ContextBudgetExceeded> {
    let b = &inp.model.estimation_budgets;
    let limit = input_limit(inp.model, inp.max_output_tokens_applied)?;
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
        return Err(ContextBudgetExceeded);
    }
    let budget = limit - deductions;

    let images = i64::try_from(inp.image_count).unwrap_or(0) * i64::from(b.image_token_budget);
    let mandatory = est(&inp.system_instructions, b) + est(inp.user_message, b) + images;
    if mandatory > budget {
        return Err(ContextBudgetExceeded);
    }
    let mut remaining = budget - mandatory;
    let mut assembled = mandatory;

    let (summary, summary_tokens) = match inp.summary {
        Some(s) if !s.trim().is_empty() => {
            let text = format!("{SUMMARY_PREAMBLE}\n\n{s}");
            let t = est(&text, b);
            if t <= remaining {
                remaining -= t;
                assembled += t;
                (Some(text), Some(t))
            } else {
                (None, None)
            }
        }
        _ => (None, None),
    };

    // Walk newest to oldest while the messages fit.
    let total = inp.recent.len();
    let mut keep_from = total;
    for (i, m) in inp.recent.iter().enumerate().rev() {
        let t = est(&m.content, b);
        if t > remaining {
            break;
        }
        remaining -= t;
        assembled += t;
        keep_from = i;
    }
    let mut kept: Vec<HistoryMessage> = inp.recent[keep_from..].to_vec();
    // Never send an answer without its question: drop leading assistants.
    while kept.first().is_some_and(|m| m.role == "assistant") {
        let m = kept.remove(0);
        assembled -= est(&m.content, b);
    }
    let messages_truncated = kept.len() < total;

    Ok(ContextPlan {
        instructions: inp.system_instructions,
        summary,
        summary_token_estimate: summary_tokens,
        recent: kept,
        messages_truncated,
        assembled_context_tokens: assembled,
        effective_budget: limit,
    })
}

/// System instructions: the model system prompt plus the tool guards.
#[must_use]
pub fn system_instructions(
    model: &ModelCatalogEntry,
    tools: ToolSet,
    knowledge_search: bool,
    web_guard: &str,
    file_guard: &str,
    knowledge_guard: &str,
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
    if knowledge_search && !knowledge_guard.is_empty() {
        parts.push(knowledge_guard);
    }
    parts.join("\n\n")
}

/// Thread summary trigger: truncation, or no summary yet and the assembled
/// context reaches `threshold_pct` of the effective budget.
#[must_use]
pub fn summary_trigger(plan: &ContextPlan, has_summary: bool, threshold_pct: u32) -> bool {
    if plan.messages_truncated {
        return true;
    }
    if has_summary {
        return false;
    }
    #[allow(clippy::integer_division)] // the threshold is floored by definition
    let threshold = i128::from(plan.effective_budget) * i128::from(threshold_pct) / 100;
    i128::from(plan.assembled_context_tokens) >= threshold
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
