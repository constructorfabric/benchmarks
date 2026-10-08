#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::ModelCatalogEntry;

use super::*;
use crate::domain::model::MessageRole;
use crate::domain::services::context::{ContextInput, ContextPlan, HistoryMessage, assemble};
use crate::infra::llm::LlmUsage;
use crate::testing::catalog::standard_model;

// ---- parse_summary ------------------------------------------------------------

#[test]
fn parse_summary_extracts_summary_block_and_collapses_blank_lines() {
    let text = "<analysis>\nthinking about it\n\n\nmore</analysis>\n\n<summary>\n1. Purpose: plan a trip\n\n\n\n2. Key info: Rome\n   \n\n3. Open items: none\n</summary>\ntrailing";
    assert_eq!(
        parse_summary(text),
        "1. Purpose: plan a trip\n\n2. Key info: Rome\n\n3. Open items: none"
    );
}

#[test]
fn parse_summary_without_tags_keeps_text() {
    assert_eq!(
        parse_summary("  The user plans a trip.\n\n\n\nThey like Rome.  \n"),
        "The user plans a trip.\n\nThey like Rome."
    );
    // The analysis block is removed even without a summary block.
    assert_eq!(
        parse_summary("<analysis>reasoning</analysis>\nPlain summary"),
        "Plain summary"
    );
}

#[test]
fn parse_summary_with_dangling_markup_is_empty() {
    assert_eq!(parse_summary("<analysis>never closed\nsome text"), "");
    assert_eq!(parse_summary("<summary>never closed"), "");
    assert_eq!(parse_summary("<analysis>a</analysis><summary"), "");
    assert_eq!(parse_summary("<summary>\n\n</summary>"), "");
    assert_eq!(parse_summary("   \n\n"), "");
}

// ---- build_summary_prompt -----------------------------------------------------

#[test]
fn prompt_without_summary_starts_with_the_plain_opening() {
    let p = build_summary_prompt(
        None,
        &[
            (MessageRole::User, "Hi".to_owned()),
            (MessageRole::Assistant, "Hello!".to_owned()),
        ],
        4000,
    );
    assert!(
        p.starts_with("Summarize the following conversation:\n\nUser: Hi\n\nAssistant: Hello!\n\n"),
        "{p}"
    );
    assert!(!p.contains("<existing_summary>"), "{p}");
    assert!(p.ends_with(ANALYSIS_INSTRUCTION), "{p}");
}

#[test]
fn prompt_contains_existing_summary_block_and_entries() {
    let p = build_summary_prompt(
        Some("Earlier: the user likes Rome."),
        &[
            (MessageRole::User, "Book a hotel".to_owned()),
            (MessageRole::Assistant, "Which dates?".to_owned()),
        ],
        4000,
    );
    let expected = format!(
        "{MERGE_OPENING}\n\n<existing_summary>\nEarlier: the user likes Rome.\n</existing_summary>\n\nNew messages to incorporate:\n\nUser: Book a hotel\n\nAssistant: Which dates?\n\n{ANALYSIS_INSTRUCTION}"
    );
    assert_eq!(p, expected);
    assert!(MERGE_OPENING.starts_with(
        "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise."
    ));
    assert!(ANALYSIS_INSTRUCTION.starts_with(
        "Before providing your final summary, wrap your analysis in <analysis> tags."
    ));
    assert!(
        ANALYSIS_INSTRUCTION
            .ends_with("Respond with an <analysis> block followed by a <summary> block.")
    );
}

#[test]
fn message_content_truncated_with_ellipsis() {
    let p = build_summary_prompt(
        None,
        &[
            (MessageRole::User, "abcdefghij".to_owned()),
            (
                MessageRole::Assistant,
                "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}".to_owned(),
            ),
            (MessageRole::User, "short".to_owned()),
        ],
        5,
    );
    assert!(p.contains("User: abcde...\n\n"), "{p}");
    // Characters, not bytes: exactly 5 characters is not cut.
    assert!(
        p.contains("Assistant: \u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\n\n"),
        "{p}"
    );
    assert!(p.contains("User: short\n\n"), "{p}");

    // 0 = no truncation.
    let p = build_summary_prompt(None, &[(MessageRole::User, "abcdefghij".to_owned())], 0);
    assert!(p.contains("User: abcdefghij\n\n"), "{p}");
}

// ---- token estimate -------------------------------------------------------------

#[test]
fn token_estimate_rules() {
    let usage = |output, reasoning| LlmUsage {
        input_tokens: 100,
        output_tokens: output,
        reasoning_tokens: reasoning,
        ..LlmUsage::default()
    };
    // output - reasoning when positive.
    assert_eq!(summary_token_estimate(Some(&usage(200, 50)), "x"), 150);
    // Otherwise ceil(bytes / 4).
    assert_eq!(summary_token_estimate(Some(&usage(0, 0)), "abcdefghi"), 3);
    assert_eq!(summary_token_estimate(Some(&usage(40, 40)), "abcd"), 1);
    assert_eq!(summary_token_estimate(Some(&usage(10, 30)), "abcde"), 2);
    assert_eq!(summary_token_estimate(None, "\u{e9}\u{e9}\u{e9}"), 2);
}

// ---- fitting ------------------------------------------------------------------------

fn msgs(n: usize, bytes: usize) -> Vec<(MessageRole, String)> {
    (0..n)
        .map(|i| {
            let role = if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            };
            (role, format!("{i:02}{}", "x".repeat(bytes - 2)))
        })
        .collect()
}

fn summary_entry(context_window: u32, max_output: u32, max_input: u32) -> ModelCatalogEntry {
    let mut e = standard_model("sum");
    e.context_window = context_window;
    e.max_output_tokens = max_output;
    e.max_input_tokens = max_input;
    e
}

#[test]
fn fit_drops_oldest_fifth_until_the_prompt_fits_keeping_two() {
    let all = msgs(10, 400);
    // No fitting when the window is unknown.
    let kept = fit_messages(&summary_entry(0, 100, 0), "sys", None, all.clone(), 0);
    assert_eq!(kept.len(), 10);

    // Large budget: everything fits.
    let kept = fit_messages(
        &summary_entry(100_000, 1000, 0),
        "sys",
        None,
        all.clone(),
        0,
    );
    assert_eq!(kept.len(), 10);

    // Budget exactly fitting the newest 6 messages (4 bytes per token):
    // 10 -> drop 2 -> 8 -> drop 2 -> 6 (oldest first).
    let tokens = |n: usize| {
        let bytes = "sys".len() + build_summary_prompt(None, &all[10 - n..], 0).len();
        u32::try_from(bytes.div_ceil(4)).unwrap()
    };
    let six = tokens(6);
    let kept = fit_messages(
        &summary_entry(1000 + six, 1000, 0),
        "sys",
        None,
        all.clone(),
        0,
    );
    assert_eq!(kept.len(), 6, "{kept:?}");
    assert!(kept[0].1.starts_with("04"));
    let kept = fit_messages(
        &summary_entry(1000 + six - 1, 1000, 0),
        "sys",
        None,
        all.clone(),
        0,
    );
    assert_eq!(kept.len(), 4);

    // max_input_tokens caps the budget.
    let kept = fit_messages(
        &summary_entry(100_000, 1000, six),
        "sys",
        None,
        all.clone(),
        0,
    );
    assert_eq!(kept.len(), 6);

    // The existing summary counts toward the prompt size.
    let summary = "s".repeat(4000);
    let kept = fit_messages(
        &summary_entry(1000 + six, 1000, 0),
        "sys",
        Some(&summary),
        all.clone(),
        0,
    );
    assert!(kept.len() < 6);

    // Never below two messages.
    let kept = fit_messages(&summary_entry(1010, 1000, 0), "sys", None, all, 0);
    assert_eq!(kept.len(), 2);
    assert!(kept[0].1.starts_with("08"));
}

// ---- trigger --------------------------------------------------------------------------

fn plan(assembled: i64, budget: i64, truncated: bool) -> ContextPlan {
    ContextPlan {
        instructions: String::new(),
        messages: Vec::new(),
        summary_applied: None,
        messages_truncated: truncated,
        assembled_context_tokens: assembled,
        effective_budget: budget,
    }
}

#[test]
fn should_trigger_proactive_only_without_summary() {
    assert!(should_trigger(&plan(800, 1000, false), false, 80));
    assert!(!should_trigger(&plan(799, 1000, false), false, 80));
    assert!(!should_trigger(&plan(1000, 1000, false), true, 80));
}

#[test]
fn should_trigger_urgent_on_truncation() {
    assert!(should_trigger(&plan(10, 1000, true), true, 80));
    assert!(should_trigger(&plan(10, 1000, true), false, 80));
}

#[test]
fn should_trigger_from_an_assembled_plan() {
    let mut entry = standard_model("m");
    entry.context_window = 3000;
    entry.max_output_tokens = 1000;
    let input = |history: Vec<HistoryMessage>| ContextInput {
        entry: entry.clone(),
        max_output_tokens_applied: 1000,
        system_prompt_extra: Vec::new(),
        summary: None,
        history,
        user_text: "x".repeat(2200),
        image_file_ids: Vec::new(),
        surcharges: 0,
    };
    let first = assemble(&input(Vec::new())).unwrap();
    assert!(!should_trigger(&first, false, 80));
    let second = assemble(&input(vec![
        HistoryMessage {
            role: MessageRole::User,
            content: "x".repeat(2200),
        },
        HistoryMessage {
            role: MessageRole::Assistant,
            content: "Hello".to_owned(),
        },
    ]))
    .unwrap();
    assert!(should_trigger(&second, false, 80));
}

// ---- prompt-too-long ----------------------------------------------------------------

#[test]
fn context_length_errors_are_detected() {
    use crate::infra::llm::LlmError;
    let provider = |m: &str| LlmError::Provider {
        message: m.to_owned(),
    };
    assert!(is_context_length_error(&provider(
        "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens."
    )));
    assert!(is_context_length_error(&provider(
        "context_length_exceeded"
    )));
    assert!(!is_context_length_error(&provider("injected failure 500")));
    assert!(!is_context_length_error(&LlmError::Timeout(
        "context length".to_owned()
    )));
}

#[test]
fn ptl_drop_count_drops_a_fifth_keeping_two() {
    assert_eq!(ptl_drop_count(10), Some(2));
    assert_eq!(ptl_drop_count(8), Some(2));
    assert_eq!(ptl_drop_count(6), Some(2));
    assert_eq!(ptl_drop_count(4), Some(1));
    assert_eq!(ptl_drop_count(3), Some(1));
    // Never below two messages: nothing left to drop.
    assert_eq!(ptl_drop_count(2), None);
    assert_eq!(ptl_drop_count(1), None);
    assert_eq!(ptl_drop_count(0), None);
}
