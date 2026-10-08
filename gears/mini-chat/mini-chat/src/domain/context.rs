//! Context plan assembly and deterministic truncation (DESIGN §4
//! "Context Plan Assembly and Truncation").

use mini_chat_sdk::EstimationBudgets;

use super::estimate::estimate_text_tokens;

/// Preamble prepended to the thread summary in the next turn's context.
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// A history message candidate (chronological order).
#[derive(Debug, Clone)]
pub struct HistoryMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ContextInput<'a> {
    /// System prompt plus tool guards.
    pub instructions: String,
    pub summary_text: Option<String>,
    /// Recent messages in chronological order (already limited to K).
    pub recent: Vec<HistoryMessage>,
    pub user_message: String,
    pub num_images: u32,
    pub budgets: &'a EstimationBudgets,
    /// `token_budget` (input limit minus surcharges and fixed overhead).
    pub token_budget: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    pub instructions: String,
    /// Summary message content (preamble + summary) when kept.
    pub summary_message: Option<String>,
    pub history: Vec<(Role, String)>,
    pub user_message: String,
    pub assembled_tokens: i64,
    pub messages_truncated: bool,
    pub summary_kept: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("mandatory context does not fit the token budget")]
pub struct ContextBudgetExceeded;

/// Input limit `min(max_input_tokens, context_window - max_output_applied)`
/// (`max_input_tokens = 0` means no separate limit). `None` when the output
/// cap does not leave room for any input.
#[must_use]
pub fn input_limit(
    context_window: u32,
    max_input_tokens: u32,
    max_output_applied: i64,
) -> Option<i64> {
    let cw = i64::from(context_window);
    if max_output_applied >= cw {
        return None;
    }
    let by_window = cw - max_output_applied;
    Some(if max_input_tokens > 0 {
        by_window.min(i64::from(max_input_tokens))
    } else {
        by_window
    })
}

fn est(text: &str, b: &EstimationBudgets) -> i64 {
    estimate_text_tokens(text.len(), b)
}

#[must_use]
pub fn summary_message(summary: &str) -> String {
    format!("{SUMMARY_PREAMBLE}\n\n{summary}")
}

/// Assemble the context plan.
///
/// # Errors
/// [`ContextBudgetExceeded`] when the mandatory items exceed the budget.
#[allow(
    clippy::suspicious_operation_groupings,
    reason = "false positive: images times per-image budget"
)]
pub fn assemble(input: &ContextInput<'_>) -> Result<ContextPlan, ContextBudgetExceeded> {
    let b = input.budgets;
    if input.token_budget <= 0 {
        return Err(ContextBudgetExceeded);
    }
    let instructions = if input.instructions.is_empty() {
        0
    } else {
        est(&input.instructions, b)
    };
    let mandatory = instructions
        + est(&input.user_message, b)
        + i64::from(input.num_images) * i64::from(b.image_token_budget);
    if mandatory > input.token_budget {
        return Err(ContextBudgetExceeded);
    }
    let mut remaining = input.token_budget - mandatory;
    let mut assembled = mandatory;

    let mut summary_message_out = None;
    if let Some(s) = &input.summary_text {
        let msg = summary_message(s);
        let t = est(&msg, b);
        if t <= remaining {
            remaining -= t;
            assembled += t;
            summary_message_out = Some(msg);
        }
    }

    // Walk newest to oldest while the messages fit.
    let mut kept: Vec<(Role, String)> = Vec::new();
    let mut dropped = false;
    for m in input.recent.iter().rev() {
        let t = est(&m.content, b);
        if t <= remaining {
            remaining -= t;
            assembled += t;
            kept.push((m.role, m.content.clone()));
        } else {
            dropped = true;
            break;
        }
    }
    kept.reverse();
    // Never start with an answer without its question.
    while kept.first().is_some_and(|(r, _)| *r == Role::Assistant) {
        let (_, c) = kept.remove(0);
        assembled -= est(&c, b);
        dropped = true;
    }
    let summary_kept = summary_message_out.is_some();
    Ok(ContextPlan {
        instructions: input.instructions.clone(),
        summary_message: summary_message_out,
        history: kept,
        user_message: input.user_message.clone(),
        assembled_tokens: assembled,
        messages_truncated: dropped,
        summary_kept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msgs(n: usize, size: usize) -> Vec<HistoryMessage> {
        (0..n)
            .map(|i| HistoryMessage {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: "x".repeat(size),
            })
            .collect()
    }

    #[test]
    fn input_limit_rules() {
        assert_eq!(input_limit(4096, 3072, 1024), Some(3072));
        assert_eq!(input_limit(4096, 0, 1024), Some(3072));
        assert_eq!(input_limit(4096, 2000, 1024), Some(2000));
        assert_eq!(input_limit(1000, 0, 1000), None);
    }

    #[test]
    fn leading_assistant_counts_as_truncation() {
        let b = EstimationBudgets::default();
        let mut recent = msgs(4, 10);
        recent.remove(0); // window starts with an assistant message
        let p = assemble(&ContextInput {
            instructions: String::new(),
            summary_text: None,
            recent,
            user_message: "hi".into(),
            num_images: 0,
            budgets: &b,
            token_budget: 100_000,
        })
        .unwrap();
        assert_eq!(p.history.first().map(|(r, _)| *r), Some(Role::User));
        assert!(p.messages_truncated);
        // Empty instructions are not sent and not charged.
        assert_eq!(
            p.assembled_tokens,
            3 * est(&"x".repeat(10), &b) - est(&"x".repeat(10), &b) + est("hi", &b)
        );
    }

    #[test]
    fn keeps_everything_when_it_fits() {
        let b = EstimationBudgets::default();
        let p = assemble(&ContextInput {
            instructions: "sys".into(),
            summary_text: Some("summary".into()),
            recent: msgs(4, 10),
            user_message: "hi".into(),
            num_images: 0,
            budgets: &b,
            token_budget: 100_000,
        })
        .unwrap();
        assert_eq!(p.history.len(), 4);
        assert!(p.summary_kept);
        assert!(!p.messages_truncated);
    }

    #[test]
    fn mandatory_overflow_is_rejected() {
        let b = EstimationBudgets::default();
        let r = assemble(&ContextInput {
            instructions: "s".repeat(10_000),
            summary_text: None,
            recent: vec![],
            user_message: "hi".into(),
            num_images: 0,
            budgets: &b,
            token_budget: 1000,
        });
        assert_eq!(r, Err(ContextBudgetExceeded));
    }

    #[test]
    fn truncates_oldest_whole_turns_first() {
        let b = EstimationBudgets::default();
        // each message: ceil(400/4)=100+100 = 200 *1.1 = 220 tokens
        let recent = msgs(6, 400);
        let mandatory = estimate_text_tokens(3, &b) + estimate_text_tokens(2, &b);
        let p = assemble(&ContextInput {
            instructions: "sys".into(),
            summary_text: None,
            recent,
            user_message: "hi".into(),
            num_images: 0,
            budgets: &b,
            token_budget: mandatory + 220 * 3,
        })
        .unwrap();
        assert!(p.messages_truncated);
        // 3 newest fit: [assistant(3)? ...] -> leading assistant dropped
        assert_eq!(p.history.first().map(|(r, _)| *r), Some(Role::User));
        assert_eq!(p.history.len(), 2);
        // deterministic
        let p2 = assemble(&ContextInput {
            instructions: "sys".into(),
            summary_text: None,
            recent: msgs(6, 400),
            user_message: "hi".into(),
            num_images: 0,
            budgets: &b,
            token_budget: mandatory + 220 * 3,
        })
        .unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn summary_dropped_when_it_does_not_fit() {
        let b = EstimationBudgets::default();
        let mandatory = estimate_text_tokens(3, &b) + estimate_text_tokens(2, &b);
        let p = assemble(&ContextInput {
            instructions: "sys".into(),
            summary_text: Some("y".repeat(10_000)),
            recent: vec![],
            user_message: "hi".into(),
            num_images: 0,
            budgets: &b,
            token_budget: mandatory + 10,
        })
        .unwrap();
        assert!(!p.summary_kept);
    }
}
