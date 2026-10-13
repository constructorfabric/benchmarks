//! Context plan assembly and truncation (DESIGN §4 "Context Plan Assembly").

use mini_chat_sdk::ModelCatalogEntry;

use super::error::DomainError;
use super::quota::{ToolSelection, estimate_item_tokens};
use crate::infra::llm::InputMessage;

pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// A history message (chronological order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

/// Inputs of context assembly.
#[derive(Debug, Clone)]
pub struct ContextInput<'a> {
    pub model: &'a ModelCatalogEntry,
    pub max_output_tokens_applied: i64,
    pub tools: ToolSelection,
    pub web_search_guard: &'a str,
    pub file_search_guard: &'a str,
    pub summary: Option<(&'a str, i64)>,
    pub recent: &'a [HistoryMessage],
    pub user_message: &'a str,
    pub image_file_ids: &'a [String],
}

/// Assembled request context.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextPlan {
    pub instructions: String,
    pub input: Vec<InputMessage>,
    /// Stored token estimate of the summary when it was included.
    pub summary_applied: Option<i64>,
    pub messages_truncated: bool,
    /// System prompt + summary + kept history + current message.
    pub assembled_tokens: i64,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
}

/// `min(max_input_tokens, context_window - max_output_applied)`; `max_input_tokens = 0` means no separate limit.
#[must_use]
pub fn input_limit(model: &ModelCatalogEntry, max_output_applied: i64) -> i64 {
    let by_window = i64::from(model.context_window) - max_output_applied;
    if model.max_input_tokens > 0 {
        by_window.min(i64::from(model.max_input_tokens))
    } else {
        by_window
    }
}

/// System prompt plus guards of the tools in the request.
#[must_use]
pub fn instructions(model: &ModelCatalogEntry, tools: ToolSelection, web_guard: &str, file_guard: &str) -> String {
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

/// Assembles the context plan deterministically.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items do not fit.
pub fn assemble(input: &ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let m = input.model;
    let b = &m.estimation_budgets;
    let limit = input_limit(m, input.max_output_tokens_applied);
    let mut surcharges = 0_i64;
    if input.tools.file_search {
        surcharges += i64::from(b.tool_surcharge_tokens);
    }
    if input.tools.web_search {
        surcharges += i64::from(b.web_search_surcharge_tokens);
    }
    if input.tools.code_interpreter {
        surcharges += i64::from(b.code_interpreter_surcharge_tokens);
    }
    let budget = limit - surcharges - i64::from(b.fixed_overhead_tokens);
    if input.max_output_tokens_applied >= i64::from(m.context_window) || budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let instructions = instructions(m, input.tools, input.web_search_guard, input.file_search_guard);
    let images = i64::try_from(input.image_file_ids.len()).unwrap_or(i64::MAX) * i64::from(b.image_token_budget);
    let mandatory = estimate_item_tokens(instructions.len(), b) + estimate_item_tokens(input.user_message.len(), b) + images;
    if mandatory > budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut remaining = budget - mandatory;
    let mut assembled = mandatory;

    let mut summary_msg = None;
    let mut summary_applied = None;
    if let Some((text, stored_estimate)) = input.summary {
        let full = format!("{SUMMARY_PREAMBLE}\n\n{text}");
        let t = estimate_item_tokens(full.len(), b);
        if t <= remaining {
            remaining -= t;
            assembled += t;
            summary_applied = Some(stored_estimate);
            summary_msg = Some(full);
        }
    }

    let mut kept_rev: Vec<&HistoryMessage> = Vec::new();
    for msg in input.recent.iter().rev() {
        let t = estimate_item_tokens(msg.content.len(), b);
        if t > remaining {
            break;
        }
        remaining -= t;
        assembled += t;
        kept_rev.push(msg);
    }
    let mut kept: Vec<&HistoryMessage> = kept_rev.into_iter().rev().collect();
    while kept.first().is_some_and(|m| m.role == "assistant") {
        let first = kept.remove(0);
        assembled -= estimate_item_tokens(first.content.len(), b);
    }
    let messages_truncated = kept.len() < input.recent.iter().filter(|m| m.role != "system").count();

    let mut msgs = Vec::new();
    if let Some(s) = summary_msg {
        msgs.push(InputMessage { role: "user", text: s, image_file_ids: Vec::new() });
    }
    for h in kept {
        msgs.push(InputMessage {
            role: if h.role == "assistant" { "assistant" } else { "user" },
            text: h.content.clone(),
            image_file_ids: Vec::new(),
        });
    }
    msgs.push(InputMessage {
        role: "user",
        text: input.user_message.to_owned(),
        image_file_ids: input.image_file_ids.to_vec(),
    });
    Ok(ContextPlan {
        instructions,
        input: msgs,
        summary_applied,
        messages_truncated,
        assembled_tokens: assembled,
        effective_budget: limit,
    })
}

/// Thread-summary trigger: urgent (truncated) or proactive (no summary and
/// assembled tokens reach the threshold of the effective budget).
#[must_use]
pub fn summary_trigger(plan: &ContextPlan, has_summary: bool, threshold_pct: u32) -> bool {
    if plan.messages_truncated {
        return true;
    }
    !has_summary && plan.assembled_tokens * 100 >= plan.effective_budget * i64::from(threshold_pct)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(cw: u32, max_in: u32) -> ModelCatalogEntry {
        serde_json::from_value(serde_json::json!({
            "id": "m", "provider_model_id": "m", "display_name": "m", "provider_id": "p", "tier": "standard",
            "enabled": true, "context_window": cw, "max_output_tokens": 100, "max_input_tokens": max_in,
            "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1,
            "system_prompt": "sys",
            "estimation_budgets": {"bytes_per_token_conservative": 1, "fixed_overhead_tokens": 0, "safety_margin_pct": 0,
                "image_token_budget": 10, "tool_surcharge_tokens": 50, "web_search_surcharge_tokens": 0, "code_interpreter_surcharge_tokens": 0}
        }))
        .unwrap()
    }

    fn h(role: &str, n: usize) -> HistoryMessage {
        HistoryMessage { role: role.into(), content: "x".repeat(n) }
    }

    fn input<'a>(m: &'a ModelCatalogEntry, recent: &'a [HistoryMessage], user: &'a str) -> ContextInput<'a> {
        ContextInput {
            model: m,
            max_output_tokens_applied: 100,
            tools: ToolSelection::default(),
            web_search_guard: "WEB",
            file_search_guard: "FILE",
            summary: None,
            recent,
            user_message: user,
            image_file_ids: &[],
        }
    }

    #[test]
    fn keeps_everything_when_it_fits() {
        let m = model(1000, 0);
        let recent = vec![h("user", 10), h("assistant", 10)];
        let p = assemble(&input(&m, &recent, "hello")).unwrap();
        assert_eq!(p.input.len(), 3);
        assert!(!p.messages_truncated);
        assert_eq!(p.effective_budget, 900);
        assert_eq!(p.instructions, "sys");
    }

    #[test]
    fn drops_oldest_whole_turns() {
        let m = model(300, 0); // budget 200
        let recent = vec![h("user", 80), h("assistant", 80), h("user", 40), h("assistant", 40)];
        let p = assemble(&input(&m, &recent, "hi")).unwrap();
        // sys(3) + hi(2) + 40 + 40 fit; the 80-token assistant does not.
        assert_eq!(p.input.len(), 3);
        assert_eq!(p.input[0].role, "user");
        assert!(p.messages_truncated);
    }

    #[test]
    fn never_starts_with_assistant() {
        let m = model(300, 0);
        let recent = vec![h("user", 150), h("assistant", 40), h("user", 40), h("assistant", 40)];
        let p = assemble(&input(&m, &recent, "hi")).unwrap();
        assert_eq!(p.input[0].role, "user");
        assert_eq!(p.input.len(), 3);
    }

    #[test]
    fn mandatory_over_budget_is_rejected() {
        let m = model(300, 0);
        let big = "y".repeat(500);
        assert!(matches!(assemble(&input(&m, &[], &big)), Err(DomainError::ContextBudgetExceeded)));
        let small_window = model(50, 0); // max_out 100 >= window
        assert!(matches!(assemble(&input(&small_window, &[], "a")), Err(DomainError::ContextBudgetExceeded)));
    }

    #[test]
    fn summary_dropped_when_it_does_not_fit_and_guards_follow_tools() {
        let m = model(400, 250); // budget 250
        let long = "s".repeat(400);
        let mut i = input(&m, &[], "q");
        i.summary = Some((&long, 100));
        i.tools = ToolSelection { file_search: true, web_search: true, code_interpreter: false };
        let p = assemble(&i).unwrap();
        assert!(p.summary_applied.is_none());
        assert_eq!(p.instructions, "sys\n\nFILE\n\nWEB");
        let short = "s".repeat(10);
        i.summary = Some((&short, 7));
        let p = assemble(&i).unwrap();
        assert_eq!(p.summary_applied, Some(7));
        assert!(p.input[0].text.starts_with(SUMMARY_PREAMBLE));
    }

    #[test]
    fn trigger_rules() {
        let m = model(1000, 0);
        let recent = vec![h("user", 700)];
        let p = assemble(&input(&m, &recent, "q")).unwrap();
        assert!(summary_trigger(&p, false, 50));
        assert!(!summary_trigger(&p, true, 50));
    }
}
