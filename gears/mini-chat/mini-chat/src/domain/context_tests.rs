use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    ContextInput, HistoryMessage, SUMMARY_PREAMBLE, SummaryForContext, assemble, input_too_long,
};
use crate::config::DEFAULT_WEB_SEARCH_GUARD;
use crate::domain::error::DomainError;
use crate::domain::estimation::estimate_text_tokens;
use crate::domain::test_fixtures::standard;
use crate::infra::llm::types::{ContentPart, InputMessage, Role};

const MAX_OUT: u32 = 100;

/// Estimation of an item of `bytes` bytes with the fixture's default budgets
/// (4 bytes per token, overhead 100, margin 10 %).
fn est(bytes: usize) -> i64 {
    estimate_text_tokens(bytes, &standard("m").estimation_budgets)
}

/// Model whose input budget is exactly `limit` tokens (`limit + MAX_OUT`
/// context window); the fixed overhead (100) is then deducted from it.
fn eff_with_limit(limit: u32) -> ModelCatalogEntry {
    let mut e = standard("m");
    e.context_window = limit + MAX_OUT;
    e
}

fn msg(n: u128, role: Role, content: &str) -> HistoryMessage {
    HistoryMessage {
        id: Uuid::from_u128(n),
        role,
        content: content.to_owned(),
        created_at: OffsetDateTime::from_unix_timestamp(1_700_000_000 + i64::try_from(n).unwrap())
            .unwrap(),
    }
}

fn input(eff: &ModelCatalogEntry) -> ContextInput<'_> {
    ContextInput {
        eff,
        max_output_tokens_applied: MAX_OUT,
        system_prompt: "sys",
        guards: vec![],
        summary: None,
        recent: vec![],
        current_text: "hi",
        current_images: vec![],
        surcharges: 0,
    }
}

/// u1 a1 u2 a2 u3 a3, each 40 bytes (est 121 tokens).
fn history() -> Vec<HistoryMessage> {
    let body = "a".repeat(40);
    vec![
        msg(1, Role::User, &body),
        msg(2, Role::Assistant, &body),
        msg(3, Role::User, &body),
        msg(4, Role::Assistant, &body),
        msg(5, Role::User, &body),
        msg(6, Role::Assistant, &body),
    ]
}

fn ids(plan: &super::ContextPlan) -> Vec<String> {
    plan.input
        .iter()
        .map(|m| match &m.content[0] {
            ContentPart::Text(t) => format!("{}:{}", m.role.as_str(), t.len()),
            ContentPart::Image { file_id } => file_id.clone(),
            other => panic!("unexpected content part {other:?}"),
        })
        .collect()
}

#[test]
fn order_is_system_summary_history_current() {
    let eff = standard("m");
    let mut i = input(&eff);
    i.guards = vec!["g1", "g2"];
    i.summary = Some(SummaryForContext {
        text: "the summary".to_owned(),
        token_estimate: 7,
    });
    i.recent = vec![msg(1, Role::User, "q1"), msg(2, Role::Assistant, "a1")];
    i.current_text = "q2";
    i.current_images = vec!["file_img".to_owned()];
    let plan = assemble(i).unwrap();

    assert_eq!(plan.instructions, "sys\n\ng1\n\ng2");
    assert_eq!(plan.input.len(), 4);
    assert_eq!(plan.input[0].role, Role::User);
    let ContentPart::Text(first) = &plan.input[0].content[0] else {
        panic!("summary must be text");
    };
    assert!(first.starts_with(SUMMARY_PREAMBLE));
    assert!(first.ends_with("the summary"));
    assert_eq!(plan.input[1], InputMessage::text(Role::User, "q1"));
    assert_eq!(plan.input[2], InputMessage::text(Role::Assistant, "a1"));
    assert_eq!(plan.input[3].role, Role::User);
    assert_eq!(
        plan.input[3].content,
        vec![
            ContentPart::Text("q2".to_owned()),
            ContentPart::Image {
                file_id: "file_img".to_owned()
            }
        ]
    );
    assert_eq!(plan.summary_applied, Some(7));
    assert!(!plan.messages_truncated);
    assert_eq!(plan.effective_budget, 128_000 - i64::from(MAX_OUT));
}

#[test]
fn preamble_text_is_exact() {
    assert_eq!(
        SUMMARY_PREAMBLE,
        "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after."
    );
}

#[test]
fn guards_appended_only_for_sent_tools() {
    let eff = standard("m");
    // The caller passes the guards of the tools actually sent (none here).
    let plan = assemble(input(&eff)).unwrap();
    assert_eq!(plan.instructions, "sys");

    let mut i = input(&eff);
    i.guards = vec![DEFAULT_WEB_SEARCH_GUARD];
    let plan = assemble(i).unwrap();
    assert_eq!(
        plan.instructions,
        "sys\n\nUse web_search only if the answer cannot be obtained from the provided context or your training data. Never use it for general knowledge questions. At most one web_search call per request."
    );
}

#[test]
fn assembled_tokens_sum_the_kept_items() {
    let eff = standard("m");
    let mut i = input(&eff);
    i.recent = vec![msg(1, Role::User, "abcd")];
    i.current_images = vec!["f1".to_owned(), "f2".to_owned()];
    let plan = assemble(i).unwrap();
    assert_eq!(
        plan.assembled_context_tokens,
        est(3) + est(4) + est(2) + 2 * 1000
    );
}

#[test]
fn truncates_oldest_whole_turns() {
    // Mandatory: est(3) + est(2) = 224. Budget for mandatory + 2 messages.
    let mandatory = est(3) + est(2);
    let limit = mandatory + 2 * est(40) + 10 + 100;
    let eff = eff_with_limit(u32::try_from(limit).unwrap());
    let mut i = input(&eff);
    i.recent = history();
    let plan = assemble(i).unwrap();

    assert!(plan.messages_truncated);
    // a3 (newest) and u3 kept, then current; kept range starts with a user.
    assert_eq!(
        ids(&plan),
        vec!["user:40", "assistant:40", "user:2"],
        "{plan:?}"
    );
    assert_eq!(plan.assembled_context_tokens, mandatory + 2 * est(40));
}

#[test]
fn kept_range_never_starts_with_assistant() {
    // Room for three messages: a2 u3 a3 would be kept; a2 is dropped (its
    // question u2 was cut).
    let mandatory = est(3) + est(2);
    let limit = mandatory + 3 * est(40) + 10 + 100;
    let eff = eff_with_limit(u32::try_from(limit).unwrap());
    let mut i = input(&eff);
    i.recent = history();
    // Make the history start such that three-from-the-end begins with an
    // assistant: u1 a1 u2 a2 u3 a3 -> last three = a2 u3 a3.
    let plan = assemble(i).unwrap();
    assert!(plan.messages_truncated);
    assert_eq!(plan.input.len(), 3, "{plan:?}");
    assert_eq!(plan.input[0].role, Role::User);
    assert_eq!(plan.assembled_context_tokens, mandatory + 2 * est(40));
}

#[test]
fn nothing_truncated_when_everything_fits() {
    let eff = standard("m");
    let mut i = input(&eff);
    i.recent = history();
    let plan = assemble(i).unwrap();
    assert!(!plan.messages_truncated);
    assert_eq!(plan.input.len(), 7);
}

#[test]
fn summary_dropped_when_it_does_not_fit() {
    let mandatory = est(3) + est(2);
    let summary_text = "s".repeat(400);
    let summary_size = est(SUMMARY_PREAMBLE.len() + 2 + summary_text.len());
    // Room for the mandatory items and two short messages, not the summary.
    let limit = mandatory + 2 * est(1) + 10 + 100;
    assert!(2 * est(1) + 10 < summary_size);
    let eff = eff_with_limit(u32::try_from(limit).unwrap());
    let mut i = input(&eff);
    i.summary = Some(SummaryForContext {
        text: summary_text.clone(),
        token_estimate: 120,
    });
    i.recent = vec![msg(1, Role::User, "q"), msg(2, Role::Assistant, "a")];
    let plan = assemble(i).unwrap();
    assert_eq!(plan.summary_applied, None);
    assert!(plan.input.iter().all(|m| m.content
        != vec![ContentPart::Text(format!(
            "{SUMMARY_PREAMBLE}\n\n{summary_text}"
        ))]));
    // The summary is dropped before recent messages are considered: the
    // remaining budget goes to history.
    assert_eq!(plan.input.len(), 3);

    // With room for everything the summary is kept.
    let big = standard("m");
    let mut i = input(&big);
    i.summary = Some(SummaryForContext {
        text: summary_text,
        token_estimate: 120,
    });
    assert_eq!(assemble(i).unwrap().summary_applied, Some(120));
}

#[test]
fn summary_has_priority_over_recent_messages() {
    let mandatory = est(3) + est(2);
    let summary_size = est(SUMMARY_PREAMBLE.len() + 2 + 11);
    // Room for the summary and exactly one message.
    let limit = mandatory + summary_size + est(40) + 10 + 100;
    let eff = eff_with_limit(u32::try_from(limit).unwrap());
    let mut i = input(&eff);
    i.summary = Some(SummaryForContext {
        text: "the summary".to_owned(),
        token_estimate: 50,
    });
    i.recent = history();
    let plan = assemble(i).unwrap();
    assert_eq!(plan.summary_applied, Some(50));
    // One message fits but it is an assistant message: dropped as well.
    assert!(plan.messages_truncated);
    assert_eq!(plan.input.len(), 2);
}

#[test]
fn mandatory_over_budget_is_context_budget_exceeded() {
    let mandatory = est(3) + est(2);
    // One token short.
    let limit = mandatory - 1 + 100;
    let eff = eff_with_limit(u32::try_from(limit).unwrap());
    assert!(matches!(
        assemble(input(&eff)),
        Err(DomainError::ContextBudgetExceeded)
    ));
    // Exactly enough.
    let eff = eff_with_limit(u32::try_from(limit + 1).unwrap());
    assert!(assemble(input(&eff)).is_ok());

    // Images are mandatory too.
    let eff = eff_with_limit(u32::try_from(mandatory + 100).unwrap());
    let mut i = input(&eff);
    i.current_images = vec!["f".to_owned()];
    assert!(matches!(
        assemble(i),
        Err(DomainError::ContextBudgetExceeded)
    ));

    // Surcharges reduce the budget.
    let eff = standard("m");
    let mut i = input(&eff);
    i.surcharges = 128_000;
    assert!(matches!(
        assemble(i),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn deductions_reaching_the_input_limit_are_context_budget_exceeded() {
    // limit == fixed overhead: nothing is left, even for empty items.
    let eff = eff_with_limit(100);
    let mut i = input(&eff);
    i.system_prompt = "";
    i.current_text = "";
    assert!(matches!(
        assemble(i),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn max_out_ge_context_window_is_context_budget_exceeded() {
    let eff = standard("m");
    for max_out in [eff.context_window, eff.context_window + 1] {
        let mut i = input(&eff);
        i.max_output_tokens_applied = max_out;
        assert!(matches!(
            assemble(i),
            Err(DomainError::ContextBudgetExceeded)
        ));
    }
}

#[test]
fn max_input_tokens_zero_means_no_limit() {
    let mut eff = standard("m");
    eff.max_input_tokens = 0;
    let text = "x".repeat(300_000);
    assert!(!input_too_long(&eff, &text));
    let plan = assemble(input(&eff)).unwrap();
    assert_eq!(plan.effective_budget, 128_000 - i64::from(MAX_OUT));

    // A limit caps the budget and bounds the message.
    eff.max_input_tokens = 1_000;
    assert!(input_too_long(&eff, &text));
    assert!(!input_too_long(&eff, "short"));
    assert_eq!(assemble(input(&eff)).unwrap().effective_budget, 1_000);

    // The window wins when it is the smaller one.
    eff.max_input_tokens = 500_000;
    assert_eq!(
        assemble(input(&eff)).unwrap().effective_budget,
        128_000 - i64::from(MAX_OUT)
    );
}

#[test]
fn input_too_long_compares_the_estimate() {
    let mut eff = standard("m");
    eff.max_input_tokens = u32::try_from(est(400)).unwrap();
    assert!(!input_too_long(&eff, &"x".repeat(400)));
    assert!(input_too_long(&eff, &"x".repeat(404)));
}

#[test]
fn deterministic_same_inputs_same_plan() {
    let eff = eff_with_limit(900);
    let mut i = input(&eff);
    i.summary = Some(SummaryForContext {
        text: "s".to_owned(),
        token_estimate: 3,
    });
    i.recent = history();
    let a = assemble(i.clone()).unwrap();
    assert_eq!(a, assemble(i.clone()).unwrap());

    // The order the caller loaded the rows in does not matter: the stable
    // key is (created_at, id).
    let mut shuffled = i.clone();
    shuffled.recent.reverse();
    assert_eq!(a, assemble(shuffled).unwrap());
}
