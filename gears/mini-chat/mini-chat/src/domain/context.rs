//! Context plan assembly and truncation (DESIGN §4 "Context Plan Assembly").

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::error::{DomainError, Res};
use crate::domain::quota::ToolGates;

pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub id: Uuid,
    pub role: HistoryRole,
    pub content: String,
    pub created_at: OffsetDateTime,
}

pub struct ContextInput<'a> {
    pub model: &'a ModelCatalogEntry,
    pub max_output_tokens_applied: u32,
    pub guards: Vec<String>,
    pub summary: Option<String>,
    /// Chronological history (already limited to `recent_messages_limit`).
    pub history: Vec<HistoryMessage>,
    pub user_message: &'a str,
    pub image_count: u32,
    pub gates: ToolGates,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    pub instructions: String,
    /// Summary message text (preamble + summary) when kept.
    pub summary_message: Option<String>,
    pub summary_tokens: i64,
    pub kept: Vec<HistoryMessage>,
    pub messages_truncated: bool,
    pub assembled_tokens: i64,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`; `None` = unlimited.
    pub effective_budget: Option<i64>,
}

#[allow(clippy::integer_division)] // truncating division + remainder correction is the point
fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

/// Token estimate of one context item (no tokenizer).
#[must_use]
pub fn item_tokens(text: &str, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(text.len()).unwrap_or(i64::MAX.div_euclid(4));
    ceil_div(ceil_div(bytes, bpt) * (100 + i64::from(b.safety_margin_pct)), 100)
}

/// Effective input budget used by context assembly and the summary threshold.
#[must_use]
pub fn effective_budget(model: &ModelCatalogEntry, max_out: u32) -> Option<i64> {
    if model.context_window == 0 {
        return None;
    }
    let window_left = i64::from(model.context_window) - i64::from(max_out);
    Some(if model.max_input_tokens > 0 {
        window_left.min(i64::from(model.max_input_tokens))
    } else {
        window_left
    })
}

fn budget_exceeded(desc: &str) -> DomainError {
    DomainError::out_of_range(Res::Chat, "content", "CONTEXT_BUDGET_EXCEEDED", desc)
}

/// Build the instructions (system prompt + tool guards).
#[must_use]
pub fn instructions(model: &ModelCatalogEntry, guards: &[String]) -> String {
    let mut s = model.system_prompt.clone();
    for g in guards {
        if g.is_empty() {
            continue;
        }
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(g);
    }
    s
}

/// Assemble and truncate deterministically.
///
/// # Errors
/// `CONTEXT_BUDGET_EXCEEDED` when the mandatory items do not fit.
pub fn assemble(input: ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let m = input.model;
    let b = &m.estimation_budgets;
    let instructions = instructions(m, &input.guards);
    let mandatory = item_tokens(&instructions, b)
        + item_tokens(input.user_message, b)
        + i64::from(input.image_count) * i64::from(b.image_token_budget);
    let summary_message = input.summary.as_ref().map(|s| format!("{SUMMARY_PREAMBLE}\n\n{s}"));
    let summary_cost = summary_message.as_deref().map_or(0, |s| item_tokens(s, b));

    let eff = effective_budget(m, input.max_output_tokens_applied);
    let Some(input_limit) = eff else {
        let assembled = mandatory + summary_cost + input.history.iter().map(|h| item_tokens(&h.content, b)).sum::<i64>();
        return Ok(ContextPlan {
            instructions,
            summary_message,
            summary_tokens: summary_cost,
            kept: input.history,
            messages_truncated: false,
            assembled_tokens: assembled,
            effective_budget: None,
        });
    };
    if i64::from(input.max_output_tokens_applied) >= i64::from(m.context_window) {
        return Err(budget_exceeded("max_output_tokens leaves no input budget"));
    }
    let mut deductions = i64::from(b.fixed_overhead_tokens);
    if input.gates.file_search {
        deductions += i64::from(b.tool_surcharge_tokens);
    }
    if input.gates.web_search {
        deductions += i64::from(b.web_search_surcharge_tokens);
    }
    if input.gates.code_interpreter {
        deductions += i64::from(b.code_interpreter_surcharge_tokens);
    }
    if deductions >= input_limit {
        return Err(budget_exceeded("tool surcharges exceed the input budget"));
    }
    let token_budget = input_limit - deductions;
    if mandatory > token_budget {
        return Err(budget_exceeded("system prompt and message exceed the context budget"));
    }
    let mut remaining = token_budget - mandatory;
    let mut assembled = mandatory;

    let (summary_message, summary_tokens) = if summary_message.is_some() && summary_cost <= remaining {
        remaining -= summary_cost;
        assembled += summary_cost;
        (summary_message, summary_cost)
    } else {
        (None, 0)
    };

    let mut kept_rev: Vec<HistoryMessage> = Vec::new();
    let mut truncated = false;
    for h in input.history.iter().rev() {
        let cost = item_tokens(&h.content, b);
        if cost <= remaining {
            remaining -= cost;
            kept_rev.push(h.clone());
        } else {
            truncated = true;
            break;
        }
    }
    let mut kept: Vec<HistoryMessage> = kept_rev.into_iter().rev().collect();
    if truncated {
        // Never send an answer without its question: drop leading assistant messages.
        while kept.first().is_some_and(|h| h.role == HistoryRole::Assistant) {
            kept.remove(0);
        }
    }
    assembled += kept.iter().map(|h| item_tokens(&h.content, b)).sum::<i64>();
    Ok(ContextPlan {
        instructions,
        summary_message,
        summary_tokens,
        kept,
        messages_truncated: truncated,
        assembled_tokens: assembled,
        effective_budget: Some(input_limit),
    })
}

/// `INPUT_TOO_LONG`: the current message estimate exceeds `max_input_tokens`.
#[must_use]
pub fn input_too_long(model: &ModelCatalogEntry, message: &str) -> bool {
    model.max_input_tokens > 0
        && crate::domain::credits::estimate_text_tokens(message.len(), &model.estimation_budgets)
            > i64::from(model.max_input_tokens)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn model(cw: u32, max_in: u32, max_out: u32) -> ModelCatalogEntry {
        serde_json::from_value(json!({
            "id": "m", "tier": "standard", "enabled": true, "system_prompt": "S",
            "context_window": cw, "max_input_tokens": max_in, "max_output_tokens": max_out,
            "input_tokens_credit_multiplier_micro": 1, "output_tokens_credit_multiplier_micro": 1,
            "estimation_budgets": {"bytes_per_token_conservative": 1, "fixed_overhead_tokens": 0, "safety_margin_pct": 0,
                "image_token_budget": 10, "tool_surcharge_tokens": 5, "web_search_surcharge_tokens": 5, "code_interpreter_surcharge_tokens": 5}
        }))
        .expect("model")
    }

    fn h(i: u128, role: HistoryRole, len: usize) -> HistoryMessage {
        HistoryMessage { id: Uuid::from_u128(i), role, content: "x".repeat(len), created_at: OffsetDateTime::UNIX_EPOCH }
    }

    fn input<'a>(m: &'a ModelCatalogEntry, history: Vec<HistoryMessage>, summary: Option<String>, msg: &'a str) -> ContextInput<'a> {
        ContextInput {
            model: m,
            max_output_tokens_applied: m.max_output_tokens,
            guards: vec!["G".into()],
            summary,
            history,
            user_message: msg,
            image_count: 0,
            gates: ToolGates::default(),
        }
    }

    #[test]
    fn keeps_everything_when_it_fits() {
        let m = model(1000, 0, 100);
        let p = assemble(input(&m, vec![h(1, HistoryRole::User, 10), h(2, HistoryRole::Assistant, 10)], None, "hi")).expect("ok");
        assert_eq!(p.kept.len(), 2);
        assert!(!p.messages_truncated);
        assert_eq!(p.instructions, "S\n\nG");
        assert_eq!(p.effective_budget, Some(900));
    }

    #[test]
    fn drops_oldest_whole_turns() {
        // budget = 200 - 100 = 100; mandatory = 4 ("S\n\nG") + 2 = 6; remaining 94
        let m = model(200, 0, 100);
        let hist = vec![
            h(1, HistoryRole::User, 30),
            h(2, HistoryRole::Assistant, 30),
            h(3, HistoryRole::User, 30),
            h(4, HistoryRole::Assistant, 30),
        ];
        let p = assemble(input(&m, hist, None, "hi")).expect("ok");
        assert!(p.messages_truncated);
        // newest 3 fit (90) but a kept range starting with an assistant message is trimmed
        assert_eq!(p.kept.iter().map(|k| k.id.as_u128()).collect::<Vec<_>>(), vec![3, 4]);
    }

    #[test]
    fn summary_dropped_when_it_does_not_fit_and_mandatory_never_truncated() {
        let m = model(200, 0, 100);
        let p = assemble(input(&m, vec![], Some("y".repeat(500)), "hi")).expect("ok");
        assert!(p.summary_message.is_none());
        let long = "z".repeat(200);
        let err = assemble(input(&m, vec![], None, &long)).expect_err("budget");
        assert!(matches!(err, DomainError::OutOfRange { reason, .. } if reason == "CONTEXT_BUDGET_EXCEEDED"));
    }

    #[test]
    fn summary_kept_with_preamble() {
        let m = model(10_000, 0, 100);
        let p = assemble(input(&m, vec![], Some("facts".into()), "hi")).expect("ok");
        let s = p.summary_message.expect("summary");
        assert!(s.starts_with(SUMMARY_PREAMBLE));
        assert!(s.ends_with("facts"));
    }

    #[test]
    fn deterministic() {
        let m = model(200, 0, 100);
        let hist = || vec![h(1, HistoryRole::User, 40), h(2, HistoryRole::Assistant, 40), h(3, HistoryRole::User, 40)];
        assert_eq!(assemble(input(&m, hist(), None, "q")).expect("a"), assemble(input(&m, hist(), None, "q")).expect("b"));
    }

    #[test]
    fn max_input_limits_budget_and_input_too_long() {
        let m = model(1000, 50, 100);
        assert_eq!(effective_budget(&m, 100), Some(50));
        assert!(input_too_long(&m, &"x".repeat(60)));
        assert!(!input_too_long(&m, "x"));
        let m = model(100, 0, 100);
        assert!(assemble(input(&m, vec![], None, "q")).is_err());
    }
}
