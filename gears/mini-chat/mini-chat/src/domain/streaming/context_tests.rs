//! Context plan assembly and truncation (slice S1-setup-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): the normative order, the mandatory items, the
//! summary drop, the oldest-first whole-turn truncation, the budget rejections and determinism.
//!
//! Every estimate in these tests is the byte length of the text (one byte per token, no overhead,
//! no margin; 1000 tokens per image), so a budget is a number of bytes.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-context-assembly`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-context-assembly`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    ContextAssembly, ContextMessage, ContextRole, CurrentMessage, ImageInput,
    THREAD_SUMMARY_PREAMBLE, ThreadSummaryRow, ToolDefinition, ToolKind, ToolSet, assemble_context,
};
use crate::domain::error::DomainError;
use crate::test_support::send::{FILE_GUARD, SYSTEM_PROMPT, WEB_GUARD, byte_budgets, std_model};

const MAX_OUT: i64 = 100;
const BIG: i64 = 1_000_000;

/// An entry whose `token_budget` (and `effective_budget`) is `budget` for an empty tool set.
fn entry(budget: i64) -> ModelCatalogEntry {
    std_model(&json!({
        "context_window": budget + MAX_OUT,
        "max_input_tokens": 0,
        "max_output_tokens": MAX_OUT,
        "estimation_budgets": byte_budgets(),
    }))
}

fn instant(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
}

fn msg(role: ContextRole, text: &str, secs: i64) -> ContextMessage {
    ContextMessage {
        id: Uuid::new_v4(),
        role,
        content: text.to_owned(),
        created_at: instant(secs),
    }
}

/// `count` alternating messages of 10 bytes each, starting with a user message, oldest first.
fn turns(count: usize) -> Vec<ContextMessage> {
    (0..count)
        .map(|i| {
            let role = if i % 2 == 0 {
                ContextRole::User
            } else {
                ContextRole::Assistant
            };
            msg(role, &format!("message-{i:02}"), i64::try_from(i).unwrap())
        })
        .collect()
}

fn summary(text: &str, estimate: i32) -> ThreadSummaryRow {
    ThreadSummaryRow {
        summary_text: text.to_owned(),
        token_estimate: estimate,
        frontier_created_at: instant(0),
        frontier_message_id: Uuid::new_v4(),
    }
}

fn current(content: &str) -> CurrentMessage {
    CurrentMessage {
        content: content.to_owned(),
        images: Vec::new(),
    }
}

fn image(file: &str) -> ImageInput {
    ImageInput {
        attachment_id: Uuid::new_v4(),
        provider_file_id: file.to_owned(),
        secondary_file_id: None,
    }
}

fn no_tools() -> ToolSet {
    ToolSet::default()
}

fn web_tools() -> ToolSet {
    ToolSet {
        tools: vec![ToolDefinition {
            kind: ToolKind::WebSearch,
            search_context_size: None,
        }],
        guards: vec![WEB_GUARD.to_owned()],
        include: Vec::new(),
        feature: "web_search".to_owned(),
        ..ToolSet::default()
    }
}

fn run(
    budget: i64,
    tools: &ToolSet,
    summary: Option<&ThreadSummaryRow>,
    recent: &[ContextMessage],
    current: &CurrentMessage,
) -> Result<ContextAssembly, DomainError> {
    assemble_context(&entry(budget), tools, MAX_OUT, summary, recent, current)
}

fn tokens_of(item: &super::ContextItem) -> i64 {
    i64::try_from(item.text.len()).unwrap()
}

/// `(system instructions tokens, current user item tokens)` of the mandatory items.
fn mandatory(tools: &ToolSet, current: &CurrentMessage) -> i64 {
    let plan = run(BIG, tools, None, &[], current).unwrap().plan;
    i64::try_from(plan.system_instructions.len()).unwrap() + plan.user.tokens
}

fn recent_texts(assembly: &ContextAssembly) -> Vec<String> {
    assembly
        .plan
        .recent
        .iter()
        .map(|item| item.text.clone())
        .collect()
}

fn texts(messages: &[ContextMessage]) -> Vec<String> {
    messages.iter().map(|m| m.content.clone()).collect()
}

// ── the plan and its order ───────────────────────────────────────────────────

#[test]
fn everything_fits_and_the_plan_has_the_normative_shape() {
    let history = turns(4);
    let sum = summary("the earlier conversation", 77);
    let now = current("what next?");
    let assembly = run(BIG, &no_tools(), Some(&sum), &history, &now).unwrap();

    assert!(assembly.plan.system_instructions.starts_with(SYSTEM_PROMPT));
    let item = assembly.plan.thread_summary.as_ref().expect("summary kept");
    assert_eq!(item.role, ContextRole::User);
    assert!(
        item.text.starts_with(THREAD_SUMMARY_PREAMBLE),
        "{}",
        item.text
    );
    assert!(item.text.contains("the earlier conversation"));
    assert_eq!(
        item.tokens,
        tokens_of(item),
        "the preamble counts toward the estimate"
    );
    assert_eq!(recent_texts(&assembly), texts(&history));
    for (item, source) in assembly.plan.recent.iter().zip(&history) {
        assert_eq!(item.message_id, Some(source.id));
        assert_eq!(item.role, source.role);
        assert_eq!(item.tokens, tokens_of(item));
    }
    assert_eq!(assembly.plan.user.text, "what next?");
    assert!(assembly.plan.user.images.is_empty());
    assert!(!assembly.messages_truncated);
    assert_eq!(assembly.summary_token_estimate, Some(77));
    assert_eq!(assembly.effective_budget, BIG);
}

#[test]
fn assembled_tokens_are_the_sum_of_the_kept_items() {
    let history = turns(4);
    let sum = summary("a summary", 5);
    let assembly = run(BIG, &no_tools(), Some(&sum), &history, &current("question")).unwrap();
    let plan = &assembly.plan;
    let expected = i64::try_from(plan.system_instructions.len()).unwrap()
        + plan.thread_summary.as_ref().unwrap().tokens
        + plan.recent.iter().map(|item| item.tokens).sum::<i64>()
        + plan.user.tokens;
    assert_eq!(assembly.assembled_context_tokens, expected);
}

#[test]
fn a_chat_without_summary_or_history_has_only_the_mandatory_items() {
    let assembly = run(BIG, &no_tools(), None, &[], &current("first message")).unwrap();
    assert!(assembly.plan.thread_summary.is_none());
    assert!(assembly.plan.recent.is_empty());
    assert!(!assembly.messages_truncated);
    assert_eq!(assembly.summary_token_estimate, None);
    assert_eq!(
        assembly.assembled_context_tokens,
        mandatory(&no_tools(), &current("first message"))
    );
}

#[test]
fn the_system_instructions_are_the_prompt_followed_by_the_guards_of_the_tool_set() {
    let plain = run(BIG, &no_tools(), None, &[], &current("q"))
        .unwrap()
        .plan;
    assert!(plain.system_instructions.starts_with(SYSTEM_PROMPT));
    assert!(!plain.system_instructions.contains(WEB_GUARD));
    assert!(!plain.system_instructions.contains(FILE_GUARD));

    let mut tools = web_tools();
    tools.guards = vec![FILE_GUARD.to_owned(), WEB_GUARD.to_owned()];
    let guarded = run(BIG, &tools, None, &[], &current("q")).unwrap().plan;
    let text = &guarded.system_instructions;
    assert!(text.starts_with(SYSTEM_PROMPT));
    let (prompt, file, web) = (
        text.find(SYSTEM_PROMPT).unwrap(),
        text.find(FILE_GUARD).unwrap(),
        text.find(WEB_GUARD).unwrap(),
    );
    assert!(prompt < file && file < web, "{text}");
    assert!(text.ends_with(WEB_GUARD), "{text}");
}

#[test]
fn the_current_images_follow_the_attachment_order_and_cost_a_budget_each() {
    let now = CurrentMessage {
        content: "see these".to_owned(),
        images: vec![image("file-b"), image("file-a"), image("file-c")],
    };
    let assembly = run(BIG, &no_tools(), None, &[], &now).unwrap();
    let files: Vec<&str> = assembly
        .plan
        .user
        .images
        .iter()
        .map(|i| i.provider_file_id.as_str())
        .collect();
    assert_eq!(files, vec!["file-b", "file-a", "file-c"]);
    assert_eq!(assembly.plan.user.text, "see these");
    assert_eq!(assembly.plan.user.tokens, 9 + 3 * 1_000);
}

#[test]
fn the_effective_budget_is_the_input_limit_without_surcharges() {
    let assembly = run(5_000, &web_tools(), None, &[], &current("q")).unwrap();
    assert_eq!(assembly.effective_budget, 5_000);
}

// ── mandatory items and the budget rejections ────────────────────────────────

#[test]
fn mandatory_items_that_do_not_fit_are_a_budget_rejection_and_an_exact_fit_is_not() {
    let now = current("a message of some length");
    let need = mandatory(&no_tools(), &now);
    let result = run(need - 1, &no_tools(), None, &[], &now);
    assert!(
        matches!(result, Err(DomainError::ContextBudgetExceeded)),
        "{result:?}"
    );
    assert!(run(need, &no_tools(), None, &[], &now).is_ok());
}

#[test]
fn the_current_images_are_mandatory_and_never_dropped() {
    let now = CurrentMessage {
        content: "pic".to_owned(),
        images: vec![image("file-1"), image("file-2")],
    };
    let need = mandatory(&no_tools(), &now);
    assert!(run(need, &no_tools(), None, &[], &now).is_ok());
    let result = run(need - 1, &no_tools(), None, &[], &now);
    assert!(
        matches!(result, Err(DomainError::ContextBudgetExceeded)),
        "{result:?}"
    );
}

#[test]
fn the_tool_guards_are_mandatory_too() {
    let now = current("q");
    let need = mandatory(&web_tools(), &now);
    // the web search surcharge (500) is deducted from the budget as well
    let result = run(need + 499, &web_tools(), None, &[], &now);
    assert!(
        matches!(result, Err(DomainError::ContextBudgetExceeded)),
        "{result:?}"
    );
    assert!(run(need + 500, &web_tools(), None, &[], &now).is_ok());
}

#[test]
fn an_output_cap_at_or_above_the_window_is_a_budget_rejection() {
    let tight = std_model(&json!({
        "context_window": 1_000, "max_input_tokens": 0, "max_output_tokens": 1_000,
        "estimation_budgets": byte_budgets(),
    }));
    let result = assemble_context(&tight, &no_tools(), 1_000, None, &[], &current("q"));
    assert!(
        matches!(result, Err(DomainError::ContextBudgetExceeded)),
        "{result:?}"
    );
}

// ── the thread summary ───────────────────────────────────────────────────────

#[test]
fn a_summary_that_does_not_fit_after_the_mandatory_items_is_dropped_entirely() {
    let now = current("q");
    let sum = summary(&"s".repeat(200), 77);
    let full = run(BIG, &no_tools(), Some(&sum), &[], &now).unwrap();
    let summary_tokens = full.plan.thread_summary.as_ref().unwrap().tokens;
    let base = mandatory(&no_tools(), &now);

    let kept = run(base + summary_tokens, &no_tools(), Some(&sum), &[], &now).unwrap();
    assert!(kept.plan.thread_summary.is_some());
    assert_eq!(kept.summary_token_estimate, Some(77));

    let dropped = run(
        base + summary_tokens - 1,
        &no_tools(),
        Some(&sum),
        &[],
        &now,
    )
    .unwrap();
    assert!(dropped.plan.thread_summary.is_none());
    assert_eq!(dropped.summary_token_estimate, None);
    assert_eq!(dropped.assembled_context_tokens, base);
}

#[test]
fn a_dropped_summary_leaves_its_room_to_the_recent_messages() {
    let now = current("q");
    let sum = summary(&"s".repeat(500), 77);
    let history = turns(2);
    let base = mandatory(&no_tools(), &now);
    let assembly = run(base + 20, &no_tools(), Some(&sum), &history, &now).unwrap();
    assert!(assembly.plan.thread_summary.is_none());
    assert_eq!(recent_texts(&assembly), texts(&history));
    assert!(!assembly.messages_truncated);
}

// ── the recent messages ──────────────────────────────────────────────────────

/// The budget that leaves room for exactly `keep` of the 10-byte messages next to the mandatory
/// items (no summary).
fn room_for(keep: i64, now: &CurrentMessage) -> i64 {
    mandatory(&no_tools(), now) + keep * 10
}

#[test]
fn the_oldest_whole_turns_are_dropped_first() {
    let now = current("q");
    let history = turns(6); // u0 a1 u2 a3 u4 a5
    // room for 4: u2 a3 u4 a5, no leading assistant
    let assembly = run(room_for(4, &now), &no_tools(), None, &history, &now).unwrap();
    assert_eq!(recent_texts(&assembly), texts(&history[2..]));
    assert!(assembly.messages_truncated);
}

#[test]
fn a_kept_range_never_starts_with_an_assistant_answer() {
    let now = current("q");
    let history = turns(6);
    // room for 3 would keep a3 u4 a5: the leading answer a3 loses its question and is dropped
    let three = run(room_for(3, &now), &no_tools(), None, &history, &now).unwrap();
    assert_eq!(recent_texts(&three), texts(&history[4..]));
    assert!(three.messages_truncated);
    // room for 5 would keep a1 u2 a3 u4 a5: a1 is dropped as well
    let five = run(room_for(5, &now), &no_tools(), None, &history, &now).unwrap();
    assert_eq!(recent_texts(&five), texts(&history[2..]));
    assert!(five.messages_truncated);
}

#[test]
fn room_for_everything_keeps_everything_and_sets_no_flag() {
    let now = current("q");
    let history = turns(6);
    let assembly = run(room_for(6, &now), &no_tools(), None, &history, &now).unwrap();
    assert_eq!(recent_texts(&assembly), texts(&history));
    assert!(!assembly.messages_truncated);
}

#[test]
fn no_room_at_all_drops_every_recent_message_and_sets_the_flag() {
    let now = current("q");
    let history = turns(4);
    let assembly = run(room_for(0, &now), &no_tools(), None, &history, &now).unwrap();
    assert!(assembly.plan.recent.is_empty());
    assert!(assembly.messages_truncated);
}

#[test]
fn the_walk_stops_at_the_first_message_that_does_not_fit_even_if_older_ones_would() {
    let now = current("q");
    let history = vec![
        msg(ContextRole::User, "tiny-1", 1),
        msg(ContextRole::Assistant, &"B".repeat(100), 2),
        msg(ContextRole::User, "tiny-2", 3),
        msg(ContextRole::Assistant, "tiny-3", 4),
    ];
    let budget = mandatory(&no_tools(), &now) + 20;
    let assembly = run(budget, &no_tools(), None, &history, &now).unwrap();
    assert_eq!(
        recent_texts(&assembly),
        vec!["tiny-2".to_owned(), "tiny-3".to_owned()]
    );
    assert!(assembly.messages_truncated);
}

#[test]
fn a_leading_assistant_message_is_dropped_even_without_budget_pressure() {
    let now = current("q");
    let history = vec![
        msg(ContextRole::Assistant, "orphan answer", 1),
        msg(ContextRole::User, "question", 2),
        msg(ContextRole::Assistant, "answer", 3),
    ];
    let assembly = run(BIG, &no_tools(), None, &history, &now).unwrap();
    assert_eq!(
        recent_texts(&assembly),
        vec!["question".to_owned(), "answer".to_owned()]
    );
    assert!(assembly.messages_truncated);
}

#[test]
fn the_kept_summary_and_history_share_the_budget_summary_first() {
    let now = current("q");
    let sum = summary(&"s".repeat(100), 9);
    let full = run(BIG, &no_tools(), Some(&sum), &[], &now).unwrap();
    let summary_tokens = full.plan.thread_summary.as_ref().unwrap().tokens;
    let history = turns(4);
    let budget = mandatory(&no_tools(), &now) + summary_tokens + 20;
    let assembly = run(budget, &no_tools(), Some(&sum), &history, &now).unwrap();
    assert!(assembly.plan.thread_summary.is_some());
    assert_eq!(recent_texts(&assembly), texts(&history[2..]));
    assert!(assembly.messages_truncated);
    assert_eq!(assembly.assembled_context_tokens, budget);
}

// ── determinism ──────────────────────────────────────────────────────────────

#[test]
fn the_same_inputs_always_yield_the_same_plan() {
    let history = turns(8);
    let sum = summary(&"s".repeat(64), 12);
    let now = current("repeatable");
    // the web search surcharge (500) is deducted from the input limit, so the limit is the
    // mandatory items, the surcharge, the summary and room for a few messages
    let summary_tokens = run(BIG, &web_tools(), Some(&sum), &[], &now)
        .unwrap()
        .plan
        .thread_summary
        .unwrap()
        .tokens;
    let budget = mandatory(&web_tools(), &now) + 500 + summary_tokens + 30;
    let first = run(budget, &web_tools(), Some(&sum), &history, &now).unwrap();
    for _ in 0..3 {
        let again = run(budget, &web_tools(), Some(&sum), &history, &now).unwrap();
        assert_eq!(again, first);
    }
}
