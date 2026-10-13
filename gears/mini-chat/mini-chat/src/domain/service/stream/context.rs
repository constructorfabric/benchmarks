//! Context assembly and truncation
//! (`cpt-cf-mini-chat-algo-streaming-turns-assemble-context`) and the
//! `INPUT_TOO_LONG` check of the send flow.

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};
use toolkit_macros::domain_model;

use super::inputs::ImageAttachment;
use super::ports::{HistoryMessage, HistoryRole, ThreadSummaryInfo};
use super::tools::{ToolSpec, ToolsDecision};
use crate::domain::error::DomainError;

/// The B.5.5 preamble of the thread-summary item.
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// The summary item of the plan.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryItem {
    /// `preamble + blank line + summary_text`.
    pub text: String,
}

/// The current message with its images.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentMessage {
    /// Content, as sent.
    pub content: String,
    /// Images of the request.
    pub images: Vec<ImageAttachment>,
}

/// The `ContextPlan` of a turn.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextPlan {
    /// `system_prompt` followed by the guards, joined by a blank line; empty
    /// when there is neither.
    pub instructions: String,
    /// The kept summary item.
    pub summary: Option<SummaryItem>,
    /// The kept history, chronological.
    pub history: Vec<HistoryMessage>,
    /// The current message.
    pub current: Option<CurrentMessage>,
    /// `token_estimate` of the kept summary.
    pub thread_summary_applied: Option<u32>,
    /// Sum of the estimates of the instructions, the kept summary, the kept
    /// history, the content and the images.
    pub assembled_context_tokens: i64,
    /// History messages were dropped for the budget (a dropped summary does
    /// not set it).
    pub messages_truncated: bool,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`
    /// (`max_input_tokens` counts only when greater than 0).
    pub effective_input_budget: i64,
}

/// Inputs of the assembly.
#[derive(Debug, Clone, Copy)]
pub struct ContextInput<'a> {
    /// The effective catalog entry.
    pub entry: &'a ModelCatalogEntry,
    /// `max_output_tokens_applied` of the decision.
    pub max_output_tokens_applied: i64,
    /// The tools and guards.
    pub tools: &'a ToolsDecision,
    /// The current content.
    pub content: &'a str,
    /// The current images.
    pub images: &'a [ImageAttachment],
    /// The thread summary.
    pub summary: Option<&'a ThreadSummaryInfo>,
    /// Recent messages, chronological.
    pub recent: &'a [HistoryMessage],
}

/// The text estimate of `bytes` UTF-8 bytes:
/// `ceil((ceil(bytes / ratio) + fixed_overhead) * (100 + margin) / 100)`
/// with `ratio` at least 1.
#[must_use]
pub fn estimate_text_tokens(budgets: &EstimationBudgets, bytes: u64) -> i64 {
    let ratio = u128::from(budgets.bytes_per_token_conservative.max(1));
    let base = u128::from(bytes)
        .div_ceil(ratio)
        .saturating_add(u128::from(budgets.fixed_overhead_tokens));
    let with_margin = base
        .saturating_mul(100 + u128::from(budgets.safety_margin_pct))
        .div_ceil(100);
    i64::try_from(with_margin).unwrap_or(i64::MAX)
}

/// `INPUT_TOO_LONG`: the text estimate of `content` is above the effective
/// model's `max_input_tokens` (checked only when that is greater than 0).
/// Images are not part of this check.
///
/// # Errors
///
/// [`DomainError::InputTooLong`].
pub fn check_input_length(entry: &ModelCatalogEntry, content: &str) -> Result<(), DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-if-input-too-long
    if entry.max_input_tokens > 0
        && estimate_text_tokens(&entry.estimation_budgets, content.len() as u64)
            > i64::from(entry.max_input_tokens)
    {
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-return-input-too-long
        return Err(DomainError::InputTooLong);
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-return-input-too-long
    }
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-if-input-too-long
    Ok(())
}

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-context-assembly:p1
/// Assembles the `ContextPlan`.
///
/// # Errors
///
/// [`DomainError::ContextBudgetExceeded`].
pub fn assemble_context(input: &ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let entry = input.entry;
    let budgets = &entry.estimation_budgets;
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-item-estimate
    let estimate = |text: &str| estimate_text_tokens(budgets, text.len() as u64);
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-item-estimate

    let window = i64::from(entry.context_window);
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-if-no-room
    if input.max_output_tokens_applied >= window {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-return-no-room
        return Err(DomainError::ContextBudgetExceeded);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-return-no-room
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-if-no-room
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-input-limit
    let mut limit = window - input.max_output_tokens_applied;
    if entry.max_input_tokens > 0 {
        limit = limit.min(i64::from(entry.max_input_tokens));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-input-limit
    let effective_input_budget = limit;

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-token-budget
    let surcharges: i64 = input
        .tools
        .tools
        .iter()
        .map(|tool| match tool {
            ToolSpec::FileSearch { .. } => i64::from(budgets.tool_surcharge_tokens),
            ToolSpec::WebSearch { .. } => i64::from(budgets.web_search_surcharge_tokens),
            ToolSpec::CodeInterpreter { .. } => {
                i64::from(budgets.code_interpreter_surcharge_tokens)
            }
            ToolSpec::SearchKnowledge => 0,
        })
        .sum();
    let budget = limit
        .saturating_sub(surcharges)
        .saturating_sub(i64::from(budgets.fixed_overhead_tokens));
    if budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-token-budget

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-build-instructions
    let instructions = std::iter::once(entry.system_prompt.as_str())
        .chain(input.tools.guards.iter().map(String::as_str))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-build-instructions
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-if-mandatory-over
    let instruction_tokens = if instructions.is_empty() {
        0
    } else {
        estimate(&instructions)
    };
    let content_tokens = estimate(input.content);
    let image_tokens = i64::from(budgets.image_token_budget)
        .saturating_mul(i64::try_from(input.images.len()).unwrap_or(i64::MAX));
    let mandatory = instruction_tokens
        .saturating_add(content_tokens)
        .saturating_add(image_tokens);
    if mandatory > budget {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-return-mandatory-over
        return Err(DomainError::ContextBudgetExceeded);
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-return-mandatory-over
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-if-mandatory-over
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-remaining
    let mut remaining = budget - mandatory;
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-remaining
    let mut assembled = mandatory;

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-if-summary
    let mut summary_item = None;
    let mut thread_summary_applied = None;
    if let Some(summary) = input.summary {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-keep-summary
        // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-next-turn-uses-summary
        let text = format!("{SUMMARY_PREAMBLE}\n\n{}", summary.summary_text);
        let cost = estimate(&text);
        if cost <= remaining {
            remaining -= cost;
            assembled = assembled.saturating_add(cost);
            thread_summary_applied = Some(u32::try_from(summary.token_estimate).unwrap_or(0));
            summary_item = Some(SummaryItem { text });
        }
        // @cpt-end:cpt-cf-mini-chat-flow-background-workers-long-conversation-compression:p1:inst-next-turn-uses-summary
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-keep-summary
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-if-summary

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-for-each-recent
    let mut kept: Vec<(&HistoryMessage, i64)> = Vec::new();
    let mut messages_truncated = false;
    for message in input.recent.iter().rev() {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-keep-or-drop
        let cost = estimate(&message.content);
        if cost <= remaining {
            remaining -= cost;
            kept.push((message, cost));
        } else {
            messages_truncated = true;
            break;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-keep-or-drop
    }
    kept.reverse();
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-for-each-recent
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-while-leading-assistant
    while kept
        .first()
        .is_some_and(|(message, _)| message.role == HistoryRole::Assistant)
    {
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-drop-leading-assistant
        kept.remove(0);
        messages_truncated = true;
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-drop-leading-assistant
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-while-leading-assistant
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-assembled-tokens
    assembled = kept
        .iter()
        .fold(assembled, |sum, (_, cost)| sum.saturating_add(*cost));
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-assembled-tokens

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-plan-order
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-return-plan
    Ok(ContextPlan {
        instructions,
        summary: summary_item,
        history: kept
            .into_iter()
            .map(|(message, _)| message.clone())
            .collect(),
        current: Some(CurrentMessage {
            content: input.content.to_owned(),
            images: input.images.to_vec(),
        }),
        thread_summary_applied,
        assembled_context_tokens: assembled,
        messages_truncated,
        effective_input_budget,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-return-plan
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-assemble-context:p1:inst-plan-order
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
