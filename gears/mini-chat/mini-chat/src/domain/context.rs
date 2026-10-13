//! `ContextPlan` assembly and truncation (DESIGN §4 "Context Plan Assembly").

use mini_chat_sdk::EstimationBudgets;

use crate::domain::error::DomainError;
use crate::domain::estimate::estimate_str;
use crate::infra::llm::{InputMessage, Role};

/// Preamble prepended to the thread summary (B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// One history message (chronological order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub role: Role,
    pub text: String,
}

/// Budget inputs of the effective model.
#[derive(Debug, Clone)]
pub struct BudgetInputs {
    pub context_window: i64,
    pub max_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    /// Surcharges of the tools included in the request.
    pub surcharges: i64,
    pub budgets: EstimationBudgets,
}

impl BudgetInputs {
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)` (0 = no limit).
    #[must_use]
    pub fn input_limit(&self) -> i64 {
        let window = self.context_window - self.max_output_tokens_applied;
        if self.max_input_tokens > 0 {
            self.max_input_tokens.min(window)
        } else {
            window
        }
    }
}

/// Assembled context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    /// History (summary message first, when kept) without the current user message.
    pub history: Vec<InputMessage>,
    pub summary_applied: Option<i64>,
    pub assembled_tokens: i64,
    pub messages_truncated: bool,
    /// `input_limit` (thread summary threshold base).
    pub effective_budget: i64,
}

/// Assemble and truncate deterministically.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items do not fit.
pub fn assemble(
    budget: &BudgetInputs,
    system_prompt: &str,
    user_text: &str,
    image_count: usize,
    summary: Option<&str>,
    recent: &[HistoryMessage],
) -> Result<ContextPlan, DomainError> {
    let b = &budget.budgets;
    if budget.max_output_tokens_applied >= budget.context_window {
        return Err(DomainError::ContextBudgetExceeded(
            "max output tokens exceed the model context window".into(),
        ));
    }
    let input_limit = budget.input_limit();
    let token_budget = input_limit - budget.surcharges - i64::from(b.fixed_overhead_tokens);
    if token_budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded(
            "tool surcharges exceed the model input budget".into(),
        ));
    }
    #[allow(clippy::integer_division)] // reason: saturation sentinel (half of i64::MAX), exact by design
    let images = i64::try_from(image_count).unwrap_or(i64::MAX / 2) * i64::from(b.image_token_budget);
    let mandatory = estimate_str(system_prompt, b) + estimate_str(user_text, b) + images;
    if mandatory > token_budget {
        return Err(DomainError::ContextBudgetExceeded(format!(
            "mandatory context of ~{mandatory} tokens exceeds the budget of {token_budget}"
        )));
    }
    let mut remaining = token_budget - mandatory;
    let mut used = mandatory;
    let mut history = Vec::new();
    let mut summary_applied = None;
    if let Some(s) = summary {
        let text = format!("{SUMMARY_PREAMBLE}\n\n{s}");
        let est = estimate_str(&text, b);
        if est <= remaining {
            remaining -= est;
            used += est;
            summary_applied = Some(est);
            history.push(InputMessage::text(Role::User, text));
        }
    }
    // newest → oldest while they fit
    let mut kept_rev: Vec<&HistoryMessage> = Vec::new();
    let mut truncated = false;
    for m in recent.iter().rev() {
        let est = estimate_str(&m.text, b);
        if est <= remaining {
            remaining -= est;
            used += est;
            kept_rev.push(m);
        } else {
            truncated = true;
            break;
        }
    }
    let mut kept: Vec<&HistoryMessage> = kept_rev.into_iter().rev().collect();
    // never start with an answer without its question
    while kept.first().is_some_and(|m| m.role == Role::Assistant) {
        used -= estimate_str(&kept[0].text, b);
        kept.remove(0);
        truncated = true;
    }
    history.extend(kept.into_iter().map(|m| InputMessage::text(m.role, m.text.clone())));
    Ok(ContextPlan {
        history,
        summary_applied,
        assembled_tokens: used,
        messages_truncated: truncated,
        effective_budget: input_limit,
    })
}

/// Thread-summary trigger (proactive or urgent).
#[must_use]
pub fn summary_trigger(plan: &ContextPlan, has_summary: bool, threshold_pct: u32) -> bool {
    if plan.messages_truncated {
        return true;
    }
    !has_summary && plan.assembled_tokens * 100 >= plan.effective_budget * i64::from(threshold_pct)
}

/// System prompt with the tool guards appended.
#[must_use]
pub fn system_prompt_with_guards(base: &str, guards: &[&str]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !base.trim().is_empty() {
        parts.push(base);
    }
    parts.extend(guards.iter().copied().filter(|g| !g.trim().is_empty()));
    parts.join("\n\n")
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
