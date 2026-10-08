#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::EstimationBudgets;

use super::{ContextInput, HistoryItem, SUMMARY_PREAMBLE, assemble_context, summary_trigger};
use crate::domain::enums::MessageRole;
use crate::domain::error::DomainError;
use crate::domain::ports::{ContentPart, InputItem};

/// One token per byte, no overhead, no margin; images cost 1000.
fn budgets() -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        image_token_budget: 1000,
        ..EstimationBudgets::default()
    }
}

fn h(role: MessageRole, content: &str) -> HistoryItem {
    HistoryItem {
        role,
        content: content.to_owned(),
    }
}

fn input(b: &EstimationBudgets, budget: i64) -> ContextInput<'_> {
    ContextInput {
        system_prompt: "sys",
        guards: vec![],
        summary: None,
        history: vec![],
        user_message: "hello",
        image_file_ids: vec![],
        budgets: b,
        token_budget: budget,
    }
}

fn text_of(item: &InputItem) -> (&'static str, String) {
    let InputItem::Message { role, content } = item else {
        panic!("expected a message")
    };
    let text = content
        .iter()
        .map(|p| match p {
            ContentPart::InputText(t) | ContentPart::OutputText(t) => t.clone(),
            ContentPart::InputImage { file_id } => format!("<img:{file_id}>"),
        })
        .collect::<String>();
    (role, text)
}

#[test]
fn system_and_user_never_truncated() {
    let b = budgets();
    let mut i = input(&b, 8); // sys(3) + hello(5) = 8 exactly
    i.history = vec![h(MessageRole::User, "q"), h(MessageRole::Assistant, "a")];
    i.summary = Some("sum");
    let plan = assemble_context(i).unwrap();
    assert_eq!(plan.instructions, "sys");
    assert_eq!(plan.input.len(), 1);
    assert_eq!(text_of(&plan.input[0]), ("user", "hello".to_owned()));
    assert_eq!(plan.assembled_tokens, 8);
    assert!(plan.messages_truncated);
    assert_eq!(plan.summary_token_estimate, None);
}

#[test]
fn mandatory_over_budget_rejects() {
    let b = budgets();
    let i = input(&b, 7);
    assert!(matches!(
        assemble_context(i),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn summary_dropped_when_not_fitting() {
    let b = budgets();
    let mut i = input(&b, 8 + 10);
    i.summary = Some("a long summary text");
    i.history = vec![h(MessageRole::User, "q1")];
    let plan = assemble_context(i).unwrap();
    assert_eq!(plan.summary_token_estimate, None);
    assert_eq!(plan.input.len(), 2);
    assert_eq!(text_of(&plan.input[0]), ("user", "q1".to_owned()));
    assert!(!plan.messages_truncated);
}

#[test]
fn summary_sent_as_user_message_with_preamble() {
    let b = budgets();
    let mut i = input(&b, 10_000);
    i.summary = Some("the summary");
    i.history = vec![h(MessageRole::User, "q1"), h(MessageRole::Assistant, "a1")];
    let plan = assemble_context(i).unwrap();
    assert_eq!(plan.input.len(), 4);
    let (role, text) = text_of(&plan.input[0]);
    assert_eq!(role, "user");
    assert_eq!(text, format!("{SUMMARY_PREAMBLE}\n\nthe summary"));
    assert!(SUMMARY_PREAMBLE.starts_with("This conversation has earlier messages"));
    let InputItem::Message { content, .. } = &plan.input[0] else {
        panic!("expected a message")
    };
    assert!(matches!(content[0], ContentPart::InputText(_)));
    assert_eq!(
        plan.summary_token_estimate,
        Some(i64::try_from(text.len()).unwrap())
    );
    assert_eq!(
        plan.assembled_tokens,
        3 + 5 + 2 + 2 + plan.summary_token_estimate.unwrap()
    );
    // history roles and part kinds
    let InputItem::Message { role, content } = &plan.input[2] else {
        panic!("expected a message")
    };
    assert_eq!(*role, "assistant");
    assert_eq!(content, &vec![ContentPart::OutputText("a1".to_owned())]);
    let InputItem::Message { role, content } = &plan.input[1] else {
        panic!("expected a message")
    };
    assert_eq!(*role, "user");
    assert_eq!(content, &vec![ContentPart::InputText("q1".to_owned())]);
}

#[test]
fn oldest_history_dropped_first_whole_turns() {
    let b = budgets();
    // mandatory 8; history: q1(2) a1(2) q2(2) a2(2) a3? use: [q1, a1, q2, a2]
    // budget fits last 3 items (a1, q2, a2 = 6) -> a1 leads, so it is dropped too.
    let mut i = input(&b, 8 + 6);
    i.history = vec![
        h(MessageRole::User, "q1"),
        h(MessageRole::Assistant, "a1"),
        h(MessageRole::User, "q2"),
        h(MessageRole::Assistant, "a2"),
    ];
    let plan = assemble_context(i).unwrap();
    let texts: Vec<_> = plan.input.iter().map(|x| text_of(x).1).collect();
    assert_eq!(texts, vec!["q2", "a2", "hello"]);
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_tokens, 8 + 4);
}

#[test]
fn newest_first_stops_at_first_misfit() {
    let b = budgets();
    // newest "bb" fits, "long-one" does not; older "x" must not be kept past it.
    let mut i = input(&b, 8 + 5);
    i.history = vec![
        h(MessageRole::User, "x"),
        h(MessageRole::Assistant, "long-one"),
        h(MessageRole::User, "bb"),
    ];
    let plan = assemble_context(i).unwrap();
    let texts: Vec<_> = plan.input.iter().map(|x| text_of(x).1).collect();
    assert_eq!(texts, vec!["bb", "hello"]);
    assert!(plan.messages_truncated);
}

#[test]
fn system_role_history_is_skipped() {
    let b = budgets();
    let mut i = input(&b, 10_000);
    i.history = vec![h(MessageRole::System, "ignored"), h(MessageRole::User, "q")];
    let plan = assemble_context(i).unwrap();
    assert_eq!(plan.input.len(), 2);
    assert!(!plan.messages_truncated);
}

#[test]
fn deterministic_for_same_input() {
    let b = budgets();
    let make = || {
        let mut i = input(&b, 8 + 9);
        i.summary = Some("s");
        i.history = vec![
            h(MessageRole::User, "q1"),
            h(MessageRole::Assistant, "a1"),
            h(MessageRole::User, "q2"),
            h(MessageRole::Assistant, "a2"),
        ];
        i
    };
    assert_eq!(
        assemble_context(make()).unwrap(),
        assemble_context(make()).unwrap()
    );
}

#[test]
fn guards_appended_after_system_prompt() {
    let b = budgets();
    let mut i = input(&b, 10_000);
    i.guards = vec!["G1", "", "G2"];
    let plan = assemble_context(i).unwrap();
    assert_eq!(plan.instructions, "sys\n\nG1\n\nG2");
    assert_eq!(plan.assembled_tokens, 11 + 5);

    let mut i = input(&b, 10_000);
    i.system_prompt = "";
    i.guards = vec!["G1"];
    assert_eq!(assemble_context(i).unwrap().instructions, "G1");
}

#[test]
fn images_appended_as_input_image() {
    let b = budgets();
    let mut i = input(&b, 10_000);
    i.image_file_ids = vec!["f1".to_owned(), "f2".to_owned()];
    let plan = assemble_context(i).unwrap();
    let InputItem::Message { role, content } = plan.input.last().unwrap() else {
        panic!("expected a message")
    };
    assert_eq!(*role, "user");
    assert_eq!(
        content,
        &vec![
            ContentPart::InputText("hello".to_owned()),
            ContentPart::InputImage {
                file_id: "f1".to_owned()
            },
            ContentPart::InputImage {
                file_id: "f2".to_owned()
            },
        ]
    );
    assert_eq!(plan.assembled_tokens, 3 + 5 + 2000);

    // images count toward the mandatory budget
    let mut i = input(&b, 1999);
    i.image_file_ids = vec!["f1".to_owned(), "f2".to_owned()];
    assert!(matches!(
        assemble_context(i),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn trigger_proactive_threshold() {
    assert!(summary_trigger(true, false, false, 800, 1000, 80));
    assert!(!summary_trigger(true, false, false, 799, 1000, 80));
    assert!(!summary_trigger(false, false, false, 900, 1000, 80));
}

#[test]
fn trigger_urgent_on_truncation() {
    assert!(summary_trigger(true, true, true, 10, 1000, 80));
    assert!(summary_trigger(true, false, true, 10, 1000, 80));
    assert!(!summary_trigger(false, true, true, 10, 1000, 80));
}

#[test]
fn no_trigger_when_summary_exists_without_truncation() {
    assert!(!summary_trigger(true, true, false, 990, 1000, 80));
}
