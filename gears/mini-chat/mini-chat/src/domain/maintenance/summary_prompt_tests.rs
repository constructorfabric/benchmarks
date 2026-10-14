//! The summary prompt: the system prompt order (catalog, configuration, built-in), the opening
//! with and without an `<existing_summary>` block, one `User:` or `Assistant:` entry per
//! non-system message separated by a blank line, the cut at `message_content_limit` characters on
//! a character boundary (multi-byte content, limit 0), the analysis instruction, the input budget
//! formula, the size estimate, the drop of the oldest fifth (rounded up, two kept) and the fitting
//! loop, plus the fixed range that fitting never changes.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-prompt` (`inst-pr-system`,
//! `inst-pr-no-summary`, `inst-pr-open-new`, `inst-pr-has-summary`, `inst-pr-open-merge`,
//! `inst-pr-each`, `inst-pr-entry`, `inst-pr-truncate`, `inst-pr-analysis`, `inst-pr-no-fit`,
//! `inst-pr-return-no-fit`, `inst-pr-budget`, `inst-pr-estimate`, `inst-pr-fit-loop`,
//! `inst-pr-fit-drop`, `inst-pr-range-unchanged`, `inst-pr-return`) and
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-handler` (`inst-sh-prompt`); definition of
//! done `cpt-cf-mini-chat-dod-background-maintenance-summary-execution`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::non_ascii_literal,
    reason = "multi-byte content is the subject of these tests"
)]

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::json;

use super::{
    ANALYSIS_INSTRUCTION, DEFAULT_SYSTEM_PROMPT, NEW_MESSAGES_HEADER, OPENING_MERGE, OPENING_NEW,
    RangeMessage, build_prompt, drop_oldest_fifth, estimate_tokens, input_budget,
    render_user_prompt, system_prompt,
};
use crate::domain::maintenance::settings::SummaryWorkerSettings;
use crate::test_support::maintenance::{range_msg, range_of, settings_for, summary_entry};

fn model(patch: &serde_json::Value) -> ModelCatalogEntry {
    summary_entry("m", "azure_openai", patch)
}

fn settings(prompt: &str, limit: usize) -> SummaryWorkerSettings {
    SummaryWorkerSettings {
        summary_system_prompt: prompt.to_owned(),
        message_content_limit: limit,
        ..settings_for("m")
    }
}

fn contents(messages: &[RangeMessage]) -> Vec<String> {
    messages.iter().map(|m| m.content.clone()).collect()
}

// ── the system prompt ────────────────────────────────────────────────────────

#[test]
fn the_catalog_prompt_wins_when_it_is_not_empty() {
    // inst-pr-system
    let entry = model(&json!({ "thread_summary_prompt": "catalog prompt" }));
    assert_eq!(
        system_prompt(&entry, &settings("config prompt", 4000)),
        "catalog prompt"
    );
}

#[test]
fn the_configured_prompt_is_used_when_the_catalog_prompt_is_empty() {
    // inst-pr-system
    let entry = model(&json!({ "thread_summary_prompt": "" }));
    assert_eq!(
        system_prompt(&entry, &settings("config prompt", 4000)),
        "config prompt"
    );
}

#[test]
fn the_built_in_prompt_is_used_when_both_are_empty() {
    // inst-pr-system
    let entry = model(&json!({ "thread_summary_prompt": "" }));
    assert_eq!(
        system_prompt(&entry, &settings("", 4000)),
        DEFAULT_SYSTEM_PROMPT
    );
}

#[test]
fn the_built_in_prompt_is_the_text_of_b_5_5_and_the_config_default() {
    // inst-pr-system: the built-in default is also the default of the configuration key
    assert_eq!(
        DEFAULT_SYSTEM_PROMPT,
        crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT
    );
    assert!(DEFAULT_SYSTEM_PROMPT.starts_with("You are a conversation summarizer."));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("<analysis>"));
    assert!(DEFAULT_SYSTEM_PROMPT.contains("<summary>"));
}

// ── the user prompt ──────────────────────────────────────────────────────────

#[test]
fn without_a_base_the_prompt_opens_with_the_summarize_instruction() {
    // inst-pr-no-summary, inst-pr-open-new
    let prompt = render_user_prompt(None, &range_of(2), 4000);
    assert!(prompt.starts_with(OPENING_NEW), "{prompt}");
    assert_eq!(OPENING_NEW, "Summarize the following conversation:");
    assert!(!prompt.contains("<existing_summary>"), "{prompt}");
    assert!(!prompt.contains(NEW_MESSAGES_HEADER), "{prompt}");
}

#[test]
fn with_a_base_the_prompt_carries_the_merge_instruction_and_the_existing_summary() {
    // inst-pr-has-summary, inst-pr-open-merge
    let prompt = render_user_prompt(Some("EARLIER SUMMARY TEXT"), &range_of(2), 4000);
    assert!(prompt.starts_with(OPENING_MERGE), "{prompt}");
    assert!(!prompt.contains(OPENING_NEW), "{prompt}");
    let open = prompt.find("<existing_summary>").expect("block opens");
    let text = prompt.find("EARLIER SUMMARY TEXT").expect("summary text");
    let close = prompt.find("</existing_summary>").expect("block closes");
    let header = prompt
        .find(NEW_MESSAGES_HEADER)
        .expect("new messages header");
    let first = prompt.find("User: m1").expect("first entry");
    assert!(prompt.find(OPENING_MERGE).unwrap() < open);
    assert!(open < text && text < close, "the text is inside the block");
    assert!(
        close < header && header < first,
        "the header precedes the entries"
    );
}

#[test]
fn the_merge_opening_keeps_the_summary_concise_and_prioritizes_recent_content() {
    // inst-pr-open-merge: the B.5.5 text
    assert!(OPENING_MERGE.contains("single updated summary"));
    assert!(OPENING_MERGE.contains("Keep the summary concise"));
    assert!(OPENING_MERGE.contains("current topic and recent decisions"));
    assert_eq!(NEW_MESSAGES_HEADER, "New messages to incorporate:");
}

#[test]
fn each_message_is_one_entry_in_range_order_separated_by_a_blank_line() {
    // inst-pr-each, inst-pr-entry
    let prompt = render_user_prompt(None, &range_of(3), 4000);
    assert!(
        prompt.contains("User: m1\n\nAssistant: m2\n\nUser: m3"),
        "{prompt}"
    );
}

#[test]
fn system_messages_are_skipped() {
    // inst-pr-each
    let messages = vec![
        range_msg("user", "hello", 10),
        range_msg("system", "SECRET SYSTEM TEXT", 20),
        range_msg("assistant", "hi there", 30),
    ];
    let prompt = render_user_prompt(None, &messages, 4000);
    assert!(!prompt.contains("SECRET SYSTEM TEXT"), "{prompt}");
    assert!(
        prompt.contains("User: hello\n\nAssistant: hi there"),
        "{prompt}"
    );
    assert!(!prompt.contains("System:"), "{prompt}");
}

#[test]
fn content_longer_than_the_limit_is_cut_and_ends_with_an_ellipsis() {
    // inst-pr-truncate
    let messages = vec![range_msg("user", "abcdefghij", 10)];
    let prompt = render_user_prompt(None, &messages, 5);
    assert!(prompt.contains("User: abcde..."), "{prompt}");
    assert!(!prompt.contains("abcdef"), "{prompt}");
}

#[test]
fn content_at_the_limit_is_not_cut() {
    // inst-pr-truncate: only content longer than the limit is cut
    let messages = vec![range_msg("user", "abcde", 10)];
    let prompt = render_user_prompt(None, &messages, 5);
    assert!(prompt.contains("User: abcde"), "{prompt}");
    assert!(!prompt.contains("abcde..."), "{prompt}");
    let longer = vec![range_msg("user", "abcdef", 10)];
    assert!(render_user_prompt(None, &longer, 5).contains("User: abcde..."));
}

#[test]
fn the_cut_counts_characters_and_keeps_multibyte_characters_whole() {
    // inst-pr-truncate: on a character boundary (the limit is in characters, not bytes)
    let messages = vec![range_msg("user", "ééééééé", 10)];
    let prompt = render_user_prompt(None, &messages, 3);
    assert!(prompt.contains("User: ééé..."), "{prompt}");
    assert!(!prompt.contains("éééé"), "{prompt}");
    let emoji = vec![range_msg("assistant", "😀😀😀😀", 10)];
    let prompt = render_user_prompt(None, &emoji, 2);
    assert!(prompt.contains("Assistant: 😀😀..."), "{prompt}");
}

#[test]
fn a_zero_limit_means_no_truncation() {
    // inst-pr-truncate: `message_content_limit` 0
    let long = "x".repeat(10_000);
    let messages = vec![range_msg("user", &long, 10)];
    let prompt = render_user_prompt(None, &messages, 0);
    assert!(prompt.contains(&format!("User: {long}")), "not cut");
    assert!(!prompt.contains("x..."));
}

#[test]
fn the_prompt_ends_with_the_analysis_instruction() {
    // inst-pr-analysis
    for base in [None, Some("S")] {
        let prompt = render_user_prompt(base, &range_of(2), 4000);
        assert!(prompt.ends_with(ANALYSIS_INSTRUCTION), "{prompt}");
    }
    for section in [
        "Conversation Purpose",
        "Key Information Exchanged",
        "User Requests and Preferences",
        "Open Items",
        "Current Topic",
    ] {
        assert!(ANALYSIS_INSTRUCTION.contains(section), "{section}");
    }
    assert!(ANALYSIS_INSTRUCTION.contains("<analysis>"));
    assert!(ANALYSIS_INSTRUCTION.contains("<summary>"));
}

#[test]
fn the_entries_come_before_the_analysis_instruction() {
    // inst-pr-analysis
    let prompt = render_user_prompt(None, &range_of(2), 4000);
    assert!(prompt.find("Assistant: m2").unwrap() < prompt.find(ANALYSIS_INSTRUCTION).unwrap());
}

// ── the input budget and the estimate ────────────────────────────────────────

#[test]
fn the_budget_is_the_context_window_minus_the_output_cap() {
    // inst-pr-budget
    let entry = model(&json!({ "context_window": 1000, "max_output_tokens": 200,
                                "max_input_tokens": 0 }));
    assert_eq!(input_budget(&entry), Some(800));
}

#[test]
fn a_smaller_max_input_tokens_caps_the_budget() {
    // inst-pr-budget
    let entry = model(&json!({ "context_window": 1000, "max_output_tokens": 200,
                                "max_input_tokens": 500 }));
    assert_eq!(input_budget(&entry), Some(500));
}

#[test]
fn a_larger_max_input_tokens_does_not_raise_the_budget() {
    // inst-pr-budget
    let entry = model(&json!({ "context_window": 1000, "max_output_tokens": 200,
                                "max_input_tokens": 900 }));
    assert_eq!(input_budget(&entry), Some(800));
}

#[test]
fn a_zero_max_input_tokens_means_no_cap() {
    // inst-pr-budget
    let entry = model(
        &json!({ "context_window": 50_000, "max_output_tokens": 1_000,
                                "max_input_tokens": 0 }),
    );
    assert_eq!(input_budget(&entry), Some(49_000));
}

#[test]
fn an_output_cap_above_the_window_leaves_a_zero_budget() {
    // inst-pr-budget: no underflow
    let entry = model(&json!({ "context_window": 100, "max_output_tokens": 200,
                                "max_input_tokens": 0 }));
    assert_eq!(input_budget(&entry), Some(0));
}

#[test]
fn a_zero_context_window_means_no_fitting() {
    // inst-pr-no-fit
    let entry = model(&json!({ "context_window": 0, "max_output_tokens": 200 }));
    assert_eq!(input_budget(&entry), None);
}

#[test]
fn the_estimate_is_the_rounded_up_byte_length_over_bytes_per_token() {
    // inst-pr-estimate
    assert_eq!(estimate_tokens("ab", "cde", 2), 3);
    assert_eq!(estimate_tokens("ab", "cd", 2), 2);
    assert_eq!(estimate_tokens("", "", 4), 0);
    assert_eq!(estimate_tokens("a", "", 4), 1);
    assert_eq!(estimate_tokens("abcd", "e", 4), 2);
}

#[test]
fn the_estimate_counts_utf8_bytes_not_characters() {
    // inst-pr-estimate
    assert_eq!(estimate_tokens("é", "", 1), 2);
    assert_eq!(estimate_tokens("😀", "", 1), 4);
}

#[test]
fn a_zero_bytes_per_token_counts_as_one() {
    // inst-pr-estimate: at least 1
    assert_eq!(estimate_tokens("abc", "de", 0), 5);
}

// ── dropping the oldest fifth ────────────────────────────────────────────────

#[test]
fn the_oldest_ceiling_of_a_fifth_is_dropped() {
    // inst-pr-fit-drop: ceil(n / 5), the oldest first
    for (n, kept) in [
        (10, 8),
        (5, 4),
        (6, 4),
        (7, 5),
        (11, 8),
        (20, 16),
        (4, 3),
        (3, 2),
    ] {
        let messages = range_of(n);
        let after = drop_oldest_fifth(&messages);
        assert_eq!(after.len(), kept, "n = {n}");
        assert_eq!(
            after,
            &messages[n - kept..],
            "the newest {kept} remain for n = {n}"
        );
    }
}

#[test]
fn two_or_fewer_messages_are_never_reduced() {
    // inst-pr-fit-drop: never fewer than two
    for n in 0..=2 {
        let messages = range_of(n);
        assert_eq!(drop_oldest_fifth(&messages), &messages[..], "n = {n}");
    }
}

// ── fitting ──────────────────────────────────────────────────────────────────

fn sized_messages(count: usize, bytes: usize) -> Vec<RangeMessage> {
    (1..=count)
        .map(|i| {
            let content = format!("{i:03}{}", "x".repeat(bytes - 3));
            range_msg(
                if i % 2 == 1 { "user" } else { "assistant" },
                &content,
                i64::try_from(i).unwrap() * 10,
            )
        })
        .collect()
}

/// A model whose input budget is `budget` tokens at one byte per token.
fn budget_model(budget: u32) -> ModelCatalogEntry {
    model(&json!({
        "context_window": budget + 100, "max_output_tokens": 100, "max_input_tokens": 0,
        "thread_summary_prompt": "",
        "estimation_budgets": { "bytes_per_token_conservative": 1 }
    }))
}

fn size_of(
    model: &ModelCatalogEntry,
    cfg: &SummaryWorkerSettings,
    base: Option<&str>,
    kept: &[RangeMessage],
) -> u64 {
    estimate_tokens(
        &system_prompt(model, cfg),
        &render_user_prompt(base, kept, cfg.message_content_limit),
        model.estimation_budgets.bytes_per_token_conservative,
    )
}

#[test]
fn a_prompt_within_the_budget_keeps_every_message() {
    // inst-pr-fit-loop: no drop while the estimate fits
    let cfg = settings("", 4000);
    let messages = sized_messages(10, 100);
    let entry = budget_model(100_000);
    let prompt = build_prompt(&entry, &cfg, None, &messages);
    assert_eq!(prompt.kept, messages);
    assert_eq!(prompt.system, DEFAULT_SYSTEM_PROMPT);
    assert_eq!(prompt.user, render_user_prompt(None, &messages, 4000));
}

#[test]
fn an_oversized_prompt_drops_the_oldest_fifth_until_it_fits() {
    // inst-pr-fit-loop, inst-pr-fit-drop, inst-pr-return
    let cfg = settings("", 4000);
    let messages = sized_messages(10, 100);
    let full = size_of(&budget_model(100_000), &cfg, None, &messages);
    let entry = budget_model(u32::try_from(full).unwrap() - 1);
    let prompt = build_prompt(&entry, &cfg, None, &messages);
    assert_eq!(prompt.kept.len(), 8, "ceil(10 / 5) = 2 dropped");
    assert_eq!(
        prompt.kept,
        messages[2..].to_vec(),
        "the oldest two are gone"
    );
    assert_eq!(prompt.user, render_user_prompt(None, &prompt.kept, 4000));
    assert!(
        size_of(&entry, &cfg, None, &prompt.kept)
            <= u64::from(entry.context_window - entry.max_output_tokens)
    );
}

#[test]
fn fitting_repeats_the_drop_step_by_step() {
    // inst-pr-fit-loop: 10 -> 8 -> 6 -> 4 until the estimate fits
    let cfg = settings("", 4000);
    let messages = sized_messages(10, 100);
    let four = size_of(&budget_model(100_000), &cfg, None, &messages[6..]);
    let entry = budget_model(u32::try_from(four).unwrap());
    let prompt = build_prompt(&entry, &cfg, None, &messages);
    assert_eq!(prompt.kept.len(), 4, "{:?}", contents(&prompt.kept));
    assert_eq!(prompt.kept, messages[6..].to_vec());
}

#[test]
fn at_least_two_messages_are_kept_whatever_the_budget() {
    // inst-pr-fit-loop: more than two messages are kept while dropping; never fewer than two
    let cfg = settings("", 4000);
    let messages = sized_messages(10, 100);
    let entry = budget_model(1);
    let prompt = build_prompt(&entry, &cfg, None, &messages);
    assert_eq!(prompt.kept.len(), 2);
    assert_eq!(prompt.kept, messages[8..].to_vec(), "the newest two");
}

#[test]
fn a_zero_budget_still_keeps_two_messages() {
    // inst-pr-budget, inst-pr-fit-loop
    let cfg = settings("", 4000);
    let messages = sized_messages(6, 50);
    let entry = model(&json!({ "context_window": 100, "max_output_tokens": 200,
                                "thread_summary_prompt": "" }));
    assert_eq!(build_prompt(&entry, &cfg, None, &messages).kept.len(), 2);
}

#[test]
fn the_existing_summary_counts_toward_the_estimate() {
    // inst-pr-estimate: the system prompt plus the user prompt, the existing summary included
    let cfg = settings("", 4000);
    let messages = sized_messages(10, 100);
    let without = size_of(&budget_model(100_000), &cfg, None, &messages);
    let entry = budget_model(u32::try_from(without).unwrap() + 10);
    assert_eq!(build_prompt(&entry, &cfg, None, &messages).kept.len(), 10);
    let big_summary = "S".repeat(500);
    let prompt = build_prompt(&entry, &cfg, Some(&big_summary), &messages);
    assert!(
        prompt.kept.len() < 10,
        "the 500-byte summary pushed the prompt over the budget"
    );
    assert!(prompt.user.contains(&big_summary));
}

#[test]
fn a_max_input_tokens_cap_drives_the_fitting() {
    // inst-pr-budget, inst-pr-fit-loop
    let cfg = settings("", 4000);
    let messages = sized_messages(10, 100);
    let full = size_of(&budget_model(100_000), &cfg, None, &messages);
    let entry = model(&json!({
        "context_window": 1_000_000, "max_output_tokens": 100,
        "max_input_tokens": full - 1, "thread_summary_prompt": "",
        "estimation_budgets": { "bytes_per_token_conservative": 1 }
    }));
    assert_eq!(build_prompt(&entry, &cfg, None, &messages).kept.len(), 8);
}

#[test]
fn without_a_context_window_nothing_is_fitted() {
    // inst-pr-no-fit, inst-pr-return-no-fit: the prompts and all messages, however large
    let cfg = settings("", 0);
    let messages = sized_messages(10, 5000);
    let entry = model(&json!({ "context_window": 0, "max_output_tokens": 4096,
                                "max_input_tokens": 10, "thread_summary_prompt": "" }));
    let prompt = build_prompt(&entry, &cfg, None, &messages);
    assert_eq!(prompt.kept, messages);
    assert_eq!(prompt.user, render_user_prompt(None, &messages, 0));
}

#[test]
fn fitting_never_changes_the_frozen_range_it_was_given() {
    // inst-pr-range-unchanged: the whole frozen range is the range to mark compressed even when
    // fitting dropped its oldest messages; the input slice is untouched
    let cfg = settings("", 4000);
    let messages = sized_messages(10, 100);
    let before = messages.clone();
    let prompt = build_prompt(&budget_model(1), &cfg, None, &messages);
    assert_eq!(messages, before);
    assert!(prompt.kept.len() < messages.len());
}

#[test]
fn the_system_prompt_of_the_result_follows_the_precedence() {
    // inst-pr-return
    let entry = model(&json!({ "thread_summary_prompt": "catalog wins",
                                "context_window": 0 }));
    let prompt = build_prompt(&entry, &settings("config", 4000), None, &range_of(2));
    assert_eq!(prompt.system, "catalog wins");
}
