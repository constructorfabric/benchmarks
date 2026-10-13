#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};

use super::*;
use crate::config::{
    DEFAULT_FILE_SEARCH_GUARD, DEFAULT_KNOWLEDGE_SEARCH_GUARD, DEFAULT_WEB_SEARCH_GUARD,
};
use crate::domain::model::MessageRole;
use crate::testing::catalog::premium_model;

/// One token per byte, no overhead, no margin; images cost 100.
fn entry(context_window: u32, max_input_tokens: u32) -> ModelCatalogEntry {
    let mut e = premium_model("m");
    e.context_window = context_window;
    e.max_input_tokens = max_input_tokens;
    e.system_prompt = "SYS".to_owned();
    e.estimation_budgets = EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        image_token_budget: 100,
        ..EstimationBudgets::default()
    };
    e
}

fn input(entry: ModelCatalogEntry) -> ContextInput {
    ContextInput {
        entry,
        max_output_tokens_applied: 0,
        system_prompt_extra: Vec::new(),
        summary: None,
        history: Vec::new(),
        user_text: "question".to_owned(),
        image_file_ids: Vec::new(),
        surcharges: 0,
    }
}

fn hist(role: MessageRole, content: &str) -> HistoryMessage {
    HistoryMessage {
        role,
        content: content.to_owned(),
    }
}

fn texts(plan: &ContextPlan) -> Vec<&str> {
    plan.messages.iter().map(|m| m.text.as_str()).collect()
}

fn tools() -> ToolSet {
    ToolSet {
        file_search: None,
        web_search: false,
        code_interpreter: Vec::new(),
        search_knowledge: false,
    }
}

#[test]
fn instructions_are_system_prompt_plus_guards() {
    let mut inp = input(entry(10_000, 0));
    inp.system_prompt_extra = vec!["G1".to_owned(), "G2".to_owned()];
    let plan = assemble(&inp).unwrap();
    assert_eq!(plan.instructions, "SYS\n\nG1\n\nG2");

    inp.system_prompt_extra.clear();
    assert_eq!(assemble(&inp).unwrap().instructions, "SYS");

    inp.entry.system_prompt.clear();
    inp.system_prompt_extra = vec!["G1".to_owned()];
    assert_eq!(assemble(&inp).unwrap().instructions, "G1");
}

#[test]
fn web_guard_only_when_web_search_sent() {
    let cfg = MiniChatConfig::default();
    assert!(guards_for(&tools(), &cfg).is_empty());
    let web = ToolSet {
        web_search: true,
        ..tools()
    };
    assert_eq!(
        guards_for(&web, &cfg),
        vec![DEFAULT_WEB_SEARCH_GUARD.to_owned()]
    );
    let mut custom = MiniChatConfig::default();
    custom.context.web_search_guard = "custom web".to_owned();
    assert_eq!(guards_for(&web, &custom), vec!["custom web".to_owned()]);
}

#[test]
fn file_guard_only_with_file_search() {
    let cfg = MiniChatConfig::default();
    let code_only = ToolSet {
        code_interpreter: vec!["file-1".to_owned()],
        ..tools()
    };
    assert!(guards_for(&code_only, &cfg).is_empty());
    let file = ToolSet {
        file_search: Some("vs_1".to_owned()),
        ..tools()
    };
    assert_eq!(
        guards_for(&file, &cfg),
        vec![DEFAULT_FILE_SEARCH_GUARD.to_owned()]
    );
    // Order: file_search, web_search, search_knowledge.
    let all = ToolSet {
        file_search: Some("vs_1".to_owned()),
        web_search: true,
        code_interpreter: Vec::new(),
        search_knowledge: true,
    };
    assert_eq!(
        guards_for(&all, &cfg),
        vec![
            DEFAULT_FILE_SEARCH_GUARD.to_owned(),
            DEFAULT_WEB_SEARCH_GUARD.to_owned(),
            DEFAULT_KNOWLEDGE_SEARCH_GUARD.to_owned(),
        ]
    );
}

#[test]
fn summary_sent_as_user_message_with_preamble() {
    let mut inp = input(entry(10_000, 0));
    inp.summary = Some(("the summary".to_owned(), 11));
    inp.history = vec![
        hist(MessageRole::User, "h1"),
        hist(MessageRole::Assistant, "a1"),
    ];
    let plan = assemble(&inp).unwrap();
    assert_eq!(plan.messages[0].role, MessageRole::User);
    assert!(plan.messages[0].text.starts_with(SUMMARY_PREAMBLE));
    assert_eq!(
        plan.messages[0].text,
        format!("{SUMMARY_PREAMBLE}\n\nthe summary")
    );
    assert_eq!(texts(&plan)[1..], ["h1", "a1", "question"]);
    let summary_tokens = i64::try_from(plan.messages[0].text.len()).unwrap();
    assert_eq!(plan.summary_applied, Some(summary_tokens));
    assert!(!plan.messages_truncated);
}

#[test]
fn history_then_user_message_with_images() {
    let mut inp = input(entry(10_000, 0));
    inp.history = vec![
        hist(MessageRole::User, "h1"),
        hist(MessageRole::Assistant, "a1"),
    ];
    inp.image_file_ids = vec!["file-a".to_owned(), "file-b".to_owned()];
    let plan = assemble(&inp).unwrap();
    assert_eq!(texts(&plan), ["h1", "a1", "question"]);
    assert_eq!(plan.messages[0].role, MessageRole::User);
    assert_eq!(plan.messages[1].role, MessageRole::Assistant);
    let last = plan.messages.last().unwrap();
    assert_eq!(last.role, MessageRole::User);
    assert_eq!(last.image_file_ids, ["file-a", "file-b"]);
    assert!(plan.messages[0].image_file_ids.is_empty());
    assert_eq!(plan.summary_applied, None);
    // SYS(3) + h1(2) + a1(2) + question(8) + 2 images * 100
    assert_eq!(plan.assembled_context_tokens, 3 + 2 + 2 + 8 + 200);
    assert_eq!(plan.effective_budget, 10_000);
}

#[test]
fn mandatory_over_budget_is_context_budget_exceeded() {
    // budget = 20: SYS(3) + question(8) + 1 image(100) > 20.
    let mut inp = input(entry(20, 0));
    inp.image_file_ids = vec!["f".to_owned()];
    assert!(matches!(
        assemble(&inp),
        Err(DomainError::ContextBudgetExceeded)
    ));
    // Exactly fitting (3 + 8 = 11) succeeds; one byte more fails.
    let mut inp = input(entry(11, 0));
    assert!(assemble(&inp).is_ok());
    inp.user_text.push('x');
    assert!(matches!(
        assemble(&inp),
        Err(DomainError::ContextBudgetExceeded)
    ));
    // Surcharges and the model's fixed overhead are deducted from the budget.
    let mut inp = input(entry(11, 0));
    inp.surcharges = 1;
    assert!(matches!(
        assemble(&inp),
        Err(DomainError::ContextBudgetExceeded)
    ));
    let mut inp = input(entry(1_000, 0));
    inp.entry.estimation_budgets.fixed_overhead_tokens = 1_000;
    assert!(matches!(
        assemble(&inp),
        Err(DomainError::ContextBudgetExceeded)
    ));
}

#[test]
fn max_output_ge_context_window_is_exceeded() {
    for out in [1_000, 1_001] {
        let mut inp = input(entry(1_000, 0));
        inp.max_output_tokens_applied = out;
        assert!(matches!(
            assemble(&inp),
            Err(DomainError::ContextBudgetExceeded)
        ));
    }
    let mut inp = input(entry(1_000, 0));
    inp.max_output_tokens_applied = 999;
    // budget = 1: mandatory (11) does not fit either.
    assert!(matches!(
        assemble(&inp),
        Err(DomainError::ContextBudgetExceeded)
    ));
    inp.max_output_tokens_applied = 989;
    let plan = assemble(&inp).unwrap();
    assert_eq!(plan.effective_budget, 11);
}

#[test]
fn max_input_tokens_caps_budget() {
    let mut inp = input(entry(10_000, 50));
    inp.history = vec![
        hist(MessageRole::User, &"u".repeat(30)),
        hist(MessageRole::Assistant, &"a".repeat(30)),
    ];
    let plan = assemble(&inp).unwrap();
    assert_eq!(plan.effective_budget, 50);
    // 50 - SYS(3) - question(8) = 39: the assistant message (30) fits, the user one does not,
    // and the leading assistant message is then dropped as well.
    assert_eq!(texts(&plan), ["question"]);
    assert!(plan.messages_truncated);

    // max_input_tokens = 0 means no cap.
    let mut inp = input(entry(10_000, 0));
    inp.max_output_tokens_applied = 1_000;
    assert_eq!(assemble(&inp).unwrap().effective_budget, 9_000);
    // max_input_tokens above the window-derived limit does not raise it.
    let mut inp = input(entry(10_000, 20_000));
    inp.max_output_tokens_applied = 1_000;
    assert_eq!(assemble(&inp).unwrap().effective_budget, 9_000);
}

#[test]
fn oldest_history_dropped_first_and_no_leading_assistant() {
    // budget = 3 + 8 + room.
    let history = vec![
        hist(MessageRole::User, "u1u1"),      // 4
        hist(MessageRole::Assistant, "a1a1"), // 4
        hist(MessageRole::User, "u2u2"),      // 4
        hist(MessageRole::Assistant, "a2a2"), // 4
    ];
    // Room for 10: a2, u2 kept (8), a1 does not fit -> dropped with u1.
    let mut inp = input(entry(3 + 8 + 10, 0));
    inp.history = history.clone();
    let plan = assemble(&inp).unwrap();
    assert_eq!(texts(&plan), ["u2u2", "a2a2", "question"]);
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_context_tokens, 3 + 8 + 8);

    // Room for 12: a2, u2, a1 fit; a1 leads, so it is dropped too.
    let mut inp = input(entry(3 + 8 + 12, 0));
    inp.history = history.clone();
    let plan = assemble(&inp).unwrap();
    assert_eq!(texts(&plan), ["u2u2", "a2a2", "question"]);
    assert!(plan.messages_truncated);
    assert_eq!(plan.assembled_context_tokens, 3 + 8 + 8);

    // Room for 16: everything fits.
    let mut inp = input(entry(3 + 8 + 16, 0));
    inp.history = history.clone();
    let plan = assemble(&inp).unwrap();
    assert_eq!(texts(&plan), ["u1u1", "a1a1", "u2u2", "a2a2", "question"]);
    assert!(!plan.messages_truncated);

    // Room for 3: nothing fits.
    let mut inp = input(entry(3 + 8 + 3, 0));
    inp.history = history;
    let plan = assemble(&inp).unwrap();
    assert_eq!(texts(&plan), ["question"]);
    assert!(plan.messages_truncated);
}

#[test]
fn walk_stops_at_first_message_that_does_not_fit() {
    // The older small message must not be kept after a bigger one was dropped.
    let mut inp = input(entry(3 + 8 + 6, 0));
    inp.history = vec![
        hist(MessageRole::User, "u"),             // 1
        hist(MessageRole::Assistant, "aaaaaaaa"), // 8
        hist(MessageRole::User, "uu"),            // 2
        hist(MessageRole::Assistant, "aa"),       // 2
    ];
    let plan = assemble(&inp).unwrap();
    assert_eq!(texts(&plan), ["uu", "aa", "question"]);
    assert!(plan.messages_truncated);
}

#[test]
fn summary_dropped_when_it_does_not_fit() {
    let summary_msg_len = SUMMARY_PREAMBLE.len() + 2 + "sum".len();
    let tokens = i64::try_from(summary_msg_len).unwrap();
    let mut inp = input(entry(
        3 + 8 + u32::try_from(summary_msg_len).unwrap() - 1,
        0,
    ));
    inp.summary = Some(("sum".to_owned(), 3));
    inp.history = vec![hist(MessageRole::User, "u1u1")];
    let plan = assemble(&inp).unwrap();
    assert_eq!(plan.summary_applied, None);
    assert_eq!(texts(&plan), ["u1u1", "question"]);
    // A dropped summary is not "truncated messages".
    assert!(!plan.messages_truncated);
    assert_eq!(plan.assembled_context_tokens, 3 + 4 + 8);

    // One token more room: the summary is kept (it has priority over history), so
    // the history no longer fits behind it and is truncated.
    let mut inp2 = inp;
    inp2.entry = entry(3 + 8 + u32::try_from(summary_msg_len).unwrap(), 0);
    let plan = assemble(&inp2).unwrap();
    assert_eq!(plan.summary_applied, Some(tokens));
    assert_eq!(texts(&plan)[1..], ["question"]);
    assert!(plan.messages_truncated);
    assert!(plan.messages[0].text.starts_with(SUMMARY_PREAMBLE));
}

#[test]
fn deterministic_for_same_input() {
    let mut inp = input(entry(200, 150));
    inp.summary = Some(("sum".to_owned(), 3));
    inp.system_prompt_extra = vec!["g".to_owned()];
    inp.image_file_ids = vec!["f".to_owned()];
    inp.history = (0..20)
        .map(|i| {
            let role = if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            hist(role, &format!("message number {i}"))
        })
        .collect();
    let first = assemble(&inp).unwrap();
    for _ in 0..5 {
        assert_eq!(assemble(&inp).unwrap(), first);
    }
}
