//! Context plan assembly and truncation (DESIGN "Context Plan Assembly and Truncation").

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use uuid::Uuid;

use super::error::DomainError;
use super::quota::estimate::estimate_text_tokens;
use crate::infra::llm::{ContentPart, InputItem, Role};

/// Preamble of the thread summary item (DESIGN B.5.5).
const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// A persisted message of the chat history.
#[derive(Debug, Clone)]
pub struct HistoryMessage {
    pub id: Uuid,
    pub role: Role,
    pub content: String,
    pub created_at: OffsetDateTime,
}

/// Inputs of one context assembly (all values of the effective model).
#[derive(Debug, Clone)]
pub struct ContextInput<'a> {
    pub model: &'a ModelCatalogEntry,
    pub max_output_tokens_applied: u32,
    /// Tool guard instructions appended to the system prompt.
    pub guards: Vec<String>,
    /// Sum of the surcharges of the tools in the request.
    pub tool_surcharge_tokens: i64,
    /// Thread summary text and its token estimate.
    pub summary: Option<(String, i64)>,
    /// Chronological, at most `context.recent_messages_limit`.
    pub history: Vec<HistoryMessage>,
    pub current_text: String,
    pub current_images: Vec<ContentPart>,
}

/// The assembled provider input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    pub instructions: String,
    pub input: Vec<InputItem>,
    /// Token estimate of the thread summary when it is part of `input`.
    pub summary_applied: Option<i64>,
    /// Estimate of everything sent (instructions and `input`).
    pub assembled_tokens: i64,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`, without the tool and
    /// overhead deductions (the thread summary trigger compares against it).
    pub effective_budget: i64,
    /// At least one history message was dropped.
    pub messages_truncated: bool,
}

/// Assemble the provider input of the current turn and truncate the history to the token budget.
///
/// Order: system instructions (never truncated), thread summary (dropped when it does not fit
/// after the mandatory items), the newest history messages that fit (oldest whole turns dropped
/// first, never starting with an assistant message), the current message with its images (never
/// truncated). Pure and deterministic.
///
/// # Errors
/// `ContextBudgetExceeded` when `max_output_tokens_applied >= context_window`, when the budget is
/// not positive, or when the mandatory items alone exceed it.
pub fn assemble(input: ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let ContextInput {
        model,
        max_output_tokens_applied,
        guards,
        tool_surcharge_tokens,
        summary,
        history,
        current_text,
        current_images,
    } = input;
    let budgets = &model.estimation_budgets;
    let estimate = |text: &str| estimate_text_tokens(text.len(), budgets);

    if max_output_tokens_applied >= model.context_window {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let window_limit = i64::from(model.context_window - max_output_tokens_applied);
    let input_limit = match model.max_input_tokens {
        0 => window_limit,
        max_input => window_limit.min(i64::from(max_input)),
    };
    let token_budget = input_limit
        .saturating_sub(tool_surcharge_tokens)
        .saturating_sub(i64::from(budgets.fixed_overhead_tokens));
    if token_budget <= 0 {
        return Err(DomainError::ContextBudgetExceeded);
    }

    let mut instructions = model.system_prompt.clone();
    if !guards.is_empty() {
        instructions.push_str("\n\n");
        instructions.push_str(&guards.join("\n\n"));
    }

    let image_tokens = i64::try_from(current_images.len())
        .unwrap_or(i64::MAX)
        .saturating_mul(i64::from(budgets.image_token_budget));
    let mandatory = estimate(&instructions)
        .saturating_add(estimate(&current_text))
        .saturating_add(image_tokens);
    if mandatory > token_budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut remaining = token_budget - mandatory;
    let mut assembled_tokens = mandatory;

    let mut items = Vec::with_capacity(history.len() + 2);
    let mut summary_applied = None;
    if let Some((text, token_estimate)) = summary {
        let text = format!("{SUMMARY_PREAMBLE}\n\n{text}");
        let cost = estimate(&text);
        if cost <= remaining {
            remaining -= cost;
            assembled_tokens = assembled_tokens.saturating_add(cost);
            summary_applied = Some(token_estimate);
            items.push(text_message(Role::User, text));
        }
    }

    // Newest to oldest while it fits; the first message that does not fit ends the walk.
    let mut kept_from = history.len();
    for (idx, message) in history.iter().enumerate().rev() {
        let cost = estimate(&message.content);
        if cost > remaining {
            break;
        }
        remaining -= cost;
        kept_from = idx;
    }
    // Truncation removes whole turns: an answer is never sent without its question.
    while kept_from < history.len() && history[kept_from].role == Role::Assistant {
        kept_from += 1;
    }
    let messages_truncated = kept_from > 0;
    for message in history.into_iter().skip(kept_from) {
        assembled_tokens = assembled_tokens.saturating_add(estimate(&message.content));
        items.push(text_message(message.role, message.content));
    }

    let mut parts = Vec::with_capacity(1 + current_images.len());
    parts.push(ContentPart::Text(current_text));
    parts.extend(current_images);
    items.push(InputItem::Message {
        role: Role::User,
        parts,
    });

    Ok(ContextPlan {
        instructions,
        input: items,
        summary_applied,
        assembled_tokens,
        effective_budget: input_limit,
        messages_truncated,
    })
}

fn text_message(role: Role, text: String) -> InputItem {
    InputItem::Message {
        role,
        parts: vec![ContentPart::Text(text)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::catalog::standard_model;

    /// One token per byte, no overhead: item cost == byte length.
    fn model(context_window: u32, max_input_tokens: u32) -> ModelCatalogEntry {
        let mut m = standard_model("m");
        m.system_prompt = "S".repeat(10);
        m.context_window = context_window;
        m.max_input_tokens = max_input_tokens;
        m.estimation_budgets.bytes_per_token_conservative = 1;
        m.estimation_budgets.fixed_overhead_tokens = 0;
        m.estimation_budgets.safety_margin_pct = 0;
        m.estimation_budgets.image_token_budget = 7;
        m
    }

    fn msg(n: u128, role: Role, len: usize) -> HistoryMessage {
        HistoryMessage {
            id: Uuid::from_u128(n),
            role,
            content: "x".repeat(len),
            created_at: OffsetDateTime::UNIX_EPOCH
                + time::Duration::seconds(i64::try_from(n).unwrap()),
        }
    }

    fn input(m: &ModelCatalogEntry, history: Vec<HistoryMessage>) -> ContextInput<'_> {
        ContextInput {
            model: m,
            max_output_tokens_applied: 100,
            guards: vec![],
            tool_surcharge_tokens: 0,
            summary: None,
            history,
            current_text: "c".repeat(5),
            current_images: vec![],
        }
    }

    fn text_of(item: &InputItem) -> (Role, String) {
        match item {
            InputItem::Message { role, parts } => match &parts[0] {
                ContentPart::Text(t) => (*role, t.clone()),
                other @ ContentPart::Image { .. } => panic!("unexpected part {other:?}"),
            },
            other => panic!("unexpected item {other:?}"),
        }
    }

    fn preamble_len() -> usize {
        SUMMARY_PREAMBLE.len() + 2
    }

    #[test]
    fn keeps_everything_when_it_fits() {
        let m = model(10_000, 0);
        let mut i = input(
            &m,
            vec![msg(1, Role::User, 20), msg(2, Role::Assistant, 30)],
        );
        i.summary = Some(("sum".to_owned(), 42));
        i.guards = vec!["g1".to_owned(), "g2".to_owned()];
        let plan = assemble(i).unwrap();
        assert_eq!(plan.instructions, format!("{}\n\ng1\n\ng2", "S".repeat(10)));
        assert_eq!(plan.input.len(), 4);
        let (role, summary) = text_of(&plan.input[0]);
        assert_eq!(role, Role::User);
        assert_eq!(summary, format!("{SUMMARY_PREAMBLE}\n\nsum"));
        assert_eq!(text_of(&plan.input[1]), (Role::User, "x".repeat(20)));
        assert_eq!(text_of(&plan.input[2]), (Role::Assistant, "x".repeat(30)));
        assert_eq!(text_of(&plan.input[3]), (Role::User, "c".repeat(5)));
        assert!(!plan.messages_truncated);
        assert_eq!(plan.summary_applied, Some(42));
        assert_eq!(plan.effective_budget, 10_000 - 100);
        let instructions = i64::try_from(plan.instructions.len()).unwrap();
        let summary_tokens = i64::try_from(preamble_len() + 3).unwrap();
        assert_eq!(
            plan.assembled_tokens,
            instructions + summary_tokens + 20 + 30 + 5
        );
    }

    #[test]
    fn no_guards_no_separator() {
        let m = model(10_000, 0);
        let plan = assemble(input(&m, vec![])).unwrap();
        assert_eq!(plan.instructions, "S".repeat(10));
        assert_eq!(plan.input.len(), 1);
        assert_eq!(plan.summary_applied, None);
    }

    #[test]
    fn drops_oldest_whole_turns_first() {
        // 5 messages of 10 bytes: u a u a u. The budget (47) fits instructions 10 + current 5 +
        // 3 messages (30) but not 4.
        let history = vec![
            msg(1, Role::User, 10),
            msg(2, Role::Assistant, 10),
            msg(3, Role::User, 10),
            msg(4, Role::Assistant, 10),
            msg(5, Role::User, 10),
        ];
        // max_output 100: input_limit = window - 100.
        let m = model(147, 0);
        let plan = assemble(input(&m, history.clone())).unwrap();
        // The newest 3 messages (u a u) are kept.
        assert!(plan.messages_truncated);
        assert_eq!(plan.input.len(), 4);
        assert_eq!(text_of(&plan.input[0]).0, Role::User);
        let again = assemble(input(&m, history)).unwrap();
        assert_eq!(plan, again);

        // u a u a with a budget for 3 messages: the kept range a u a starts with an assistant
        // message, which is dropped too, leaving u a.
        let history = vec![
            msg(1, Role::User, 10),
            msg(2, Role::Assistant, 10),
            msg(3, Role::User, 10),
            msg(4, Role::Assistant, 10),
        ];
        let m = model(100 + 15 + 30, 0);
        let plan = assemble(input(&m, history)).unwrap();
        assert!(plan.messages_truncated);
        assert_eq!(plan.input.len(), 3); // user, assistant, current
        assert_eq!(text_of(&plan.input[0]), (Role::User, "x".repeat(10)));
    }

    #[test]
    fn leading_assistant_is_dropped() {
        // The budget fits only the newest message (an assistant answer): it is dropped as well.
        let history = vec![msg(1, Role::User, 10), msg(2, Role::Assistant, 10)];
        let m = model(125, 0);
        let plan = assemble(input(&m, history)).unwrap();
        assert!(plan.messages_truncated);
        assert_eq!(plan.input.len(), 1);
        assert_eq!(plan.assembled_tokens, 15);
    }

    #[test]
    fn summary_dropped_when_it_alone_does_not_fit() {
        let m = model(100 + 15 + 20, 0);
        let mut i = input(&m, vec![]);
        i.summary = Some(("s".repeat(50), 60));
        let plan = assemble(i).unwrap();
        assert_eq!(plan.summary_applied, None);
        assert_eq!(plan.input.len(), 1);
        assert!(!plan.messages_truncated);
        assert_eq!(plan.assembled_tokens, 15);
    }

    #[test]
    fn mandatory_over_budget_is_context_budget_exceeded() {
        // budget 14 < 10 + 5
        let m = model(114, 0);
        assert!(matches!(
            assemble(input(&m, vec![])),
            Err(DomainError::ContextBudgetExceeded)
        ));
        // exactly 15 fits
        let m = model(115, 0);
        assert!(assemble(input(&m, vec![])).is_ok());
        // deductions reach the limit
        let m = model(10_000, 0);
        let mut i = input(&m, vec![]);
        i.tool_surcharge_tokens = 9_900;
        assert!(matches!(
            assemble(i),
            Err(DomainError::ContextBudgetExceeded)
        ));
        // overhead is deducted from the budget
        let mut m = model(115, 0);
        m.estimation_budgets.fixed_overhead_tokens = 1;
        // overhead also adds to each item estimate; 10+1 + 5+1 = 17 > 115-100-1
        assert!(matches!(
            assemble(input(&m, vec![])),
            Err(DomainError::ContextBudgetExceeded)
        ));
    }

    #[test]
    fn max_output_ge_context_window_rejected() {
        let m = model(100, 0);
        let mut i = input(&m, vec![]);
        i.max_output_tokens_applied = 100;
        assert!(matches!(
            assemble(i),
            Err(DomainError::ContextBudgetExceeded)
        ));
        let mut i = input(&m, vec![]);
        i.max_output_tokens_applied = 5_000;
        assert!(matches!(
            assemble(i),
            Err(DomainError::ContextBudgetExceeded)
        ));
    }

    #[test]
    fn max_input_tokens_caps_the_limit() {
        let m = model(100_000, 40);
        let plan = assemble(input(&m, vec![])).unwrap();
        assert_eq!(plan.effective_budget, 40);
    }

    #[test]
    fn images_cost_image_token_budget() {
        let img = || ContentPart::Image {
            file_id: "f".to_owned(),
            secondary_file_id: None,
        };
        let m = model(10_000, 0);
        let mut i = input(&m, vec![]);
        i.current_images = vec![img(), img()];
        let plan = assemble(i).unwrap();
        assert_eq!(plan.assembled_tokens, 10 + 5 + 2 * 7);
        match &plan.input[0] {
            InputItem::Message { parts, .. } => {
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[1], img());
            }
            other => panic!("unexpected item {other:?}"),
        }

        // images make the mandatory part too large: budget 15 + 14 - 1
        let m = model(100 + 28, 0);
        let mut i = input(&m, vec![]);
        i.current_images = vec![img(), img()];
        assert!(matches!(
            assemble(i),
            Err(DomainError::ContextBudgetExceeded)
        ));
    }
}
