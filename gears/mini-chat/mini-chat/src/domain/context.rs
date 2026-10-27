//! Context plan assembly and deterministic truncation (DESIGN.md "Context
//! Plan Assembly and Truncation").

use mini_chat_sdk::ModelCatalogEntry;

use crate::domain::quota_service::QuotaService;
use crate::error::DomainError;

/// A single history message in the context plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMessage {
    /// `user` | `assistant`.
    pub role: String,
    /// Message content.
    pub content: String,
}

/// The assembled context plan for one turn.
#[derive(Debug, Clone)]
pub struct ContextPlan {
    /// System prompt (with tool guards appended).
    pub system_prompt: String,
    /// Thread summary (preamble + text) and its stored token estimate, when
    /// kept.
    pub summary: Option<(String, u32)>,
    /// Recent messages (chronological).
    pub messages: Vec<ContextMessage>,
    /// Provider file ids of the current turn's images.
    pub images: Vec<String>,
    /// Whether truncation dropped at least one recent message.
    pub messages_truncated: bool,
    /// Estimated tokens of the assembled request (for the summary trigger).
    pub assembled_context_tokens: i64,
}

/// Inputs for the truncation algorithm.
pub struct AssemblyInput {
    /// System prompt including tool guard instructions.
    pub system_prompt: String,
    /// Existing thread summary: `(preamble + text, stored token_estimate)`.
    pub summary: Option<(String, u32)>,
    /// Recent messages, chronological (oldest first).
    pub recent: Vec<ContextMessage>,
    /// Current user message content.
    pub user_message: String,
    /// Number of images on the current turn.
    pub image_count: u32,
}

/// Estimate one text item in tokens with the model's estimation budgets.
#[must_use]
pub fn estimate_item_tokens(model: &ModelCatalogEntry, bytes: usize) -> i64 {
    QuotaService::estimate_text_tokens(model, bytes)
}

/// The input budget halves: `(effective_budget, token_budget)`.
///
/// `effective_budget = min(max_input_tokens, context_window -
/// max_output_tokens_applied)` (0 = no separate limit) drives the summary
/// trigger; `token_budget` additionally subtracts the tool surcharges and
/// the fixed overhead and drives truncation.
#[must_use]
pub fn budgets(
    model: &ModelCatalogEntry,
    max_output_tokens_applied: i64,
    tool_surcharges: i64,
) -> (i64, i64) {
    let window_minus_output = i64::from(model.context_window) - max_output_tokens_applied;
    let input_limit = if model.max_input_tokens == 0 {
        window_minus_output
    } else {
        i64::from(model.max_input_tokens).min(window_minus_output)
    };
    let token_budget = input_limit
        - tool_surcharges
        - i64::from(model.estimation_budgets.fixed_overhead_tokens);
    (input_limit, token_budget)
}

/// Assemble the context plan with deterministic truncation.
///
/// # Errors
///
/// Returns [`DomainError::ContextBudgetExceeded`] when the mandatory items
/// (system prompt + current message + images) alone exceed the budget.
pub fn assemble(model: &ModelCatalogEntry, max_output_tokens_applied: i64, tool_surcharges: i64, input: AssemblyInput) -> Result<ContextPlan, DomainError> {
    let (_, token_budget) = budgets(model, max_output_tokens_applied, tool_surcharges);

    let system_tokens = estimate_item_tokens(model, input.system_prompt.len());
    let user_tokens = estimate_item_tokens(model, input.user_message.len());
    let image_tokens =
        i64::from(input.image_count) * i64::from(model.estimation_budgets.image_token_budget);
    let mandatory = system_tokens + user_tokens + image_tokens;
    if mandatory > token_budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut remaining = token_budget - mandatory;

    // Thread summary: kept if it fits after the mandatory items.
    let summary = match &input.summary {
        Some((text, _estimate)) => {
            let summary_tokens = estimate_item_tokens(model, text.len());
            if summary_tokens <= remaining {
                remaining -= summary_tokens;
                Some(text.clone())
            } else {
                None
            }
        }
        None => None,
    };
    let kept_summary = summary.map(|text| {
        let estimate = input.summary.as_ref().map_or(0, |(_, e)| *e);
        (text, estimate)
    });

    // Recent messages: walked newest to oldest, kept while they fit; the
    // first message that does not fit and all older ones are dropped. If the
    // kept range would start with an assistant message, it is dropped too
    // (never an answer without its question).
    let mut kept: Vec<ContextMessage> = Vec::with_capacity(input.recent.len());
    let mut truncated = false;
    for msg in input.recent.iter().rev() {
        let tokens = estimate_item_tokens(model, msg.content.len());
        if tokens <= remaining {
            remaining -= tokens;
            kept.push(msg.clone());
        } else {
            truncated = true;
            break;
        }
    }
    kept.reverse();
    while kept.first().is_some_and(|m| m.role == "assistant") {
        kept.remove(0);
        truncated = true;
    }

    let assembled = token_budget - remaining;
    Ok(ContextPlan {
        system_prompt: input.system_prompt,
        summary: kept_summary,
        messages: kept,
        images: Vec::new(),
        messages_truncated: truncated,
        assembled_context_tokens: assembled,
    })
}

/// The thread summary trigger evaluation (DESIGN "Trigger timing").
///
/// Returns `true` when a summary task should be scheduled for the completed
/// turn.
#[must_use]
pub fn summary_trigger(
    enabled: bool,
    summary_exists: bool,
    messages_truncated: bool,
    assembled_context_tokens: i64,
    effective_budget: i64,
    compression_threshold_pct: u32,
) -> bool {
    if !enabled {
        return false;
    }
    if summary_exists && !messages_truncated {
        return false;
    }
    if messages_truncated {
        return true;
    }
    !summary_exists
        && effective_budget > 0
        && assembled_context_tokens * 100 >= i64::from(compression_threshold_pct) * effective_budget
}

/// The summary preamble + text delivered as one user-role message.
#[must_use]
pub fn summary_message_text(preamble: &str, summary: &str) -> String {
    format!("{preamble}\n\n{summary}")
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
