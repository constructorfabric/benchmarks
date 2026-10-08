//! ContextPlan assembly and deterministic truncation (DESIGN §4 "Context
//! Plan Assembly and Truncation").

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::credits::estimate_text_tokens;
use crate::domain::errors::{DomainError, DomainResult, Res};
use crate::infra::llm::{ChatItem, ItemRole};

pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// One history message considered for the context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMsg {
    pub id: Uuid,
    pub created_at: OffsetDateTime,
    pub role: String,
    pub content: String,
}

/// Input budget `min(max_input_tokens, context_window - max_output_tokens_applied)`
/// (`max_input_tokens = 0`: no separate limit; `context_window = 0`: unbounded).
#[must_use]
pub fn input_limit(model: &ModelCatalogEntry, max_output_tokens_applied: i64) -> Option<i64> {
    let cw = i64::from(model.context_window);
    let base = if cw == 0 {
        i64::MAX
    } else {
        if max_output_tokens_applied >= cw {
            return None;
        }
        cw - max_output_tokens_applied
    };
    let mi = i64::from(model.max_input_tokens);
    Some(if mi > 0 { base.min(mi) } else { base })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    /// History items (summary message first when kept), chronological.
    pub items: Vec<ChatItem>,
    pub summary_included: bool,
    pub summary_tokens: i64,
    pub messages_truncated: bool,
    /// Estimated tokens of system prompt + summary + history + user message.
    pub assembled_tokens: i64,
}

fn budget_exceeded() -> DomainError {
    DomainError::out_of_range(
        Res::Chat,
        "content",
        "CONTEXT_BUDGET_EXCEEDED",
        "the mandatory context does not fit the model's input budget",
    )
}

/// The summary message as sent to the provider.
#[must_use]
pub fn summary_message(summary: &str) -> String {
    format!("{SUMMARY_PREAMBLE}\n\n{summary}")
}

/// Assemble the context within `token_budget`.
///
/// # Errors
/// 400 `CONTEXT_BUDGET_EXCEEDED` when the mandatory items do not fit.
pub fn assemble(
    instructions: &str,
    summary: Option<&str>,
    history: &[HistoryMsg],
    user_text: &str,
    image_count: usize,
    budgets: &EstimationBudgets,
    token_budget: i64,
) -> DomainResult<ContextPlan> {
    if token_budget <= 0 {
        return Err(budget_exceeded());
    }
    let images = i64::try_from(image_count).unwrap_or(0) * i64::from(budgets.image_token_budget);
    let mandatory = estimate_text_tokens(instructions, budgets)
        .saturating_add(estimate_text_tokens(user_text, budgets))
        .saturating_add(images);
    if mandatory > token_budget {
        return Err(budget_exceeded());
    }
    let mut remaining = token_budget - mandatory;
    let mut assembled = mandatory;

    let mut summary_item = None;
    let mut summary_tokens = 0;
    if let Some(s) = summary {
        let text = summary_message(s);
        let t = estimate_text_tokens(&text, budgets);
        if t <= remaining {
            remaining -= t;
            assembled += t;
            summary_tokens = t;
            summary_item = Some(text);
        }
    }

    // Recent messages: newest to oldest while they fit.
    let mut kept: Vec<&HistoryMsg> = Vec::new();
    let mut truncated = false;
    for (i, m) in history.iter().enumerate().rev() {
        let t = estimate_text_tokens(&m.content, budgets);
        if t > remaining {
            truncated = history[..=i].iter().any(|h| h.role != "system");
            break;
        }
        remaining -= t;
        assembled += t;
        kept.push(m);
    }
    kept.reverse();
    // Never start with an assistant answer without its question.
    while truncated && kept.first().is_some_and(|m| m.role == "assistant") {
        let dropped = kept.remove(0);
        assembled -= estimate_text_tokens(&dropped.content, budgets);
        truncated = true;
    }

    let mut items = Vec::with_capacity(kept.len() + 1);
    if let Some(text) = summary_item.clone() {
        items.push(ChatItem {
            role: ItemRole::User,
            text,
            images: Vec::new(),
        });
    }
    for m in kept {
        if m.role == "system" {
            continue;
        }
        items.push(ChatItem {
            role: if m.role == "assistant" { ItemRole::Assistant } else { ItemRole::User },
            text: m.content.clone(),
            images: Vec::new(),
        });
    }
    Ok(ContextPlan {
        items,
        summary_included: summary_item.is_some(),
        summary_tokens,
        messages_truncated: truncated,
        assembled_tokens: assembled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str, i: u8) -> HistoryMsg {
        HistoryMsg {
            id: Uuid::from_bytes([i; 16]),
            created_at: OffsetDateTime::UNIX_EPOCH,
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    fn budgets() -> EstimationBudgets {
        EstimationBudgets {
            bytes_per_token_conservative: 1,
            fixed_overhead_tokens: 0,
            safety_margin_pct: 0,
            ..EstimationBudgets::default()
        }
    }

    #[test]
    fn keeps_everything_when_it_fits() {
        let h = vec![msg("user", "aaaa", 1), msg("assistant", "bbbb", 2)];
        let p = assemble("sys", Some("sum"), &h, "q", 0, &budgets(), 1000).unwrap();
        assert!(p.summary_included);
        assert_eq!(p.items.len(), 3);
        assert!(!p.messages_truncated);
        assert_eq!(p.items[1].text, "aaaa");
    }

    #[test]
    fn drops_oldest_whole_turns() {
        let h = vec![
            msg("user", "1111111111", 1),
            msg("assistant", "2222222222", 2),
            msg("user", "33", 3),
            msg("assistant", "44", 4),
        ];
        // mandatory = 3 + 1 = 4; room for 15 -> keeps "44","33" then "2222222222" (10) fits -> 14,
        // then "1111111111" does not; kept starts with assistant -> dropped.
        let p = assemble("sys", None, &h, "q", 0, &budgets(), 19).unwrap();
        assert!(p.messages_truncated);
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.items[0].text, "33");
    }

    #[test]
    fn mandatory_overflow_rejected() {
        let e = assemble("system prompt", None, &[], "a long question", 0, &budgets(), 5).unwrap_err();
        assert!(matches!(e, DomainError::OutOfRange { ref reason, .. } if reason == "CONTEXT_BUDGET_EXCEEDED"));
    }

    #[test]
    fn summary_dropped_when_too_big() {
        let p = assemble("s", Some(&"x".repeat(500)), &[], "q", 0, &budgets(), 100).unwrap();
        assert!(!p.summary_included);
    }

    #[test]
    fn input_limit_rules() {
        let mut m: ModelCatalogEntry =
            serde_json::from_value(serde_json::json!({"id":"m","tier":"standard","context_window":4096,"max_input_tokens":3072})).unwrap();
        assert_eq!(input_limit(&m, 1024), Some(3072));
        m.max_input_tokens = 0;
        assert_eq!(input_limit(&m, 1024), Some(3072));
        assert_eq!(input_limit(&m, 4096), None);
        m.context_window = 0;
        assert_eq!(input_limit(&m, 4096), Some(i64::MAX));
    }
}
