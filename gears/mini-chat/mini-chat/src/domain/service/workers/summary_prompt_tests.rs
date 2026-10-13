#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::non_ascii_literal
)]
//! Slice 5: the summary prompt, its fitting to the input budget and the oldest
//! fifth drop of the context-length retries
//! (`cpt-cf-mini-chat-algo-background-workers-build-summary-prompt`, DESIGN B.5.5).

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    ANALYSIS_INSTRUCTION, DEFAULT_SYSTEM_PROMPT, OPENING_FIRST, OPENING_MERGE, RangeMessage,
    build_prompt, drop_oldest_fifth, estimate_prompt_tokens, input_budget, rebuild_prompt,
    select_system_prompt, user_prompt,
};
use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::worker_test_env::small_model;

fn msg(n: u128, role: &str, content: &str) -> RangeMessage {
    RangeMessage {
        id: Uuid::from_u128(n),
        role: role.to_owned(),
        content: content.to_owned(),
        created_at: OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::try_from(n).unwrap())
            .unwrap(),
    }
}

/// A model with a context window of 3000 tokens and 100 output tokens: an input
/// budget of 2900 tokens (one token per byte), above the fixed ~1.3 KB of the
/// instructions.
fn entry() -> ModelCatalogEntry {
    let mut entry = small_model("m", "");
    entry.context_window = 3000;
    entry
}

fn long_range(count: u128, bytes: usize) -> Vec<RangeMessage> {
    (1..=count)
        .map(|n| {
            msg(
                n,
                if n % 2 == 1 { "user" } else { "assistant" },
                &"x".repeat(bytes),
            )
        })
        .collect()
}

// ---- the system prompt -----------------------------------------------------------------------------------------

#[test]
fn the_catalog_prompt_wins_then_the_configured_one_then_the_built_in_default() {
    assert_eq!(select_system_prompt("catalog", "configured"), "catalog");
    assert_eq!(select_system_prompt("", "configured"), "configured");
    assert_eq!(select_system_prompt("", ""), DEFAULT_SYSTEM_PROMPT);
}

#[test]
fn the_built_in_default_is_the_default_of_the_configuration() {
    assert_eq!(DEFAULT_SYSTEM_PROMPT, DEFAULT_SUMMARY_SYSTEM_PROMPT);
    assert!(
        DEFAULT_SYSTEM_PROMPT.contains("<analysis>") && DEFAULT_SYSTEM_PROMPT.contains("<summary>")
    );
}

// ---- the user prompt -------------------------------------------------------------------------------------------

#[test]
fn a_first_summary_opens_with_the_summarize_instruction_and_ends_with_the_analysis_instruction() {
    let prompt = user_prompt(
        &[msg(1, "user", "Hi"), msg(2, "assistant", "Hello")],
        None,
        0,
    );

    assert!(prompt.starts_with(OPENING_FIRST), "{prompt}");
    assert!(prompt.starts_with("Summarize the following conversation:"));
    assert!(prompt.ends_with(ANALYSIS_INSTRUCTION), "{prompt}");
    assert!(prompt.ends_with("Respond with an <analysis> block followed by a <summary> block."));
}

#[test]
fn each_message_is_one_entry_labelled_by_role_and_entries_are_separated_by_a_blank_line() {
    let prompt = user_prompt(
        &[msg(1, "user", "Hi"), msg(2, "assistant", "Hello")],
        None,
        0,
    );

    assert!(
        prompt
            .contains("Summarize the following conversation:\n\nUser: Hi\n\nAssistant: Hello\n\n"),
        "{prompt}"
    );
}

#[test]
fn messages_of_any_other_role_are_skipped() {
    let prompt = user_prompt(
        &[
            msg(1, "system", "secret system text"),
            msg(2, "user", "Hi"),
            msg(3, "tool", "tool output"),
        ],
        None,
        0,
    );

    assert!(prompt.contains("User: Hi"));
    assert!(
        !prompt.contains("secret system text") && !prompt.contains("tool output"),
        "{prompt}"
    );
}

#[test]
fn a_stored_summary_is_merged_through_the_existing_summary_block() {
    let prompt = user_prompt(&[msg(1, "user", "Hi")], Some("Earlier: X"), 0);

    assert!(prompt.starts_with(OPENING_MERGE), "{prompt}");
    assert!(
        prompt.contains("<existing_summary>\nEarlier: X\n</existing_summary>"),
        "{prompt}"
    );
    assert!(
        prompt.contains("New messages to incorporate:\n\nUser: Hi"),
        "{prompt}"
    );
    assert!(prompt.contains("IMPORTANT: Keep the summary concise."));
    assert!(!prompt.contains(OPENING_FIRST));
}

#[test]
fn content_longer_than_the_limit_is_cut_in_characters_and_ends_with_an_ellipsis() {
    let prompt = user_prompt(
        &[msg(1, "user", "abcdefghij"), msg(2, "assistant", "klmnopq")],
        None,
        5,
    );

    assert!(prompt.contains("User: abcde...\n"), "{prompt}");
    assert!(prompt.contains("Assistant: klmno..."), "{prompt}");
    assert!(!prompt.contains("fghij"));
}

#[test]
fn content_at_the_limit_is_not_cut_and_a_zero_limit_cuts_nothing() {
    let at_limit = user_prompt(&[msg(1, "user", "abcde")], None, 5);
    assert!(
        at_limit.contains("User: abcde\n") && !at_limit.contains("..."),
        "{at_limit}"
    );
    let unlimited = user_prompt(&[msg(1, "user", &"y".repeat(20_000))], None, 0);
    assert!(unlimited.contains(&"y".repeat(20_000)));
}

#[test]
fn the_limit_counts_unicode_scalar_values_not_bytes() {
    let prompt = user_prompt(&[msg(1, "user", "日本語のテキスト")], None, 3);

    assert!(prompt.contains("User: 日本語...\n"), "{prompt}");
}

// ---- the input budget and the fitting ----------------------------------------------------------------------

#[test]
fn the_budget_is_the_window_minus_the_output_lowered_to_the_input_limit() {
    let mut e = entry();
    e.context_window = 1000;
    assert_eq!(input_budget(&e), Some(900));
    e.max_input_tokens = 500;
    assert_eq!(input_budget(&e), Some(500));
    e.max_input_tokens = 2000;
    assert_eq!(
        input_budget(&e),
        Some(900),
        "an input limit above the window changes nothing"
    );
}

#[test]
fn a_zero_context_window_applies_no_fitting() {
    let mut e = entry();
    e.context_window = 0;

    assert_eq!(input_budget(&e), None);
    let prompt = build_prompt(&e, &long_range(40, 2_000), None, "sys", 0);
    assert_eq!(prompt.kept.len(), 40);
}

#[test]
fn the_estimate_is_the_bytes_over_the_conservative_ratio_rounded_up() {
    let mut e = entry();
    e.estimation_budgets.bytes_per_token_conservative = 4;

    assert_eq!(estimate_prompt_tokens(&e, "abcd", "efgh"), 2);
    assert_eq!(estimate_prompt_tokens(&e, "abcd", "efghi"), 3);
}

#[test]
fn a_range_that_fits_is_kept_whole() {
    let prompt = build_prompt(&entry(), &long_range(4, 10), None, "sys", 0);

    assert_eq!(prompt.kept.len(), 4);
    assert_eq!(prompt.system, "sys");
}

#[test]
fn a_range_over_the_budget_drops_the_oldest_fifth_until_it_fits() {
    // each message is ~100 bytes and the instructions ~1.3 KB: 2900 tokens are exceeded by 20 messages
    let range = long_range(20, 100);

    let prompt = build_prompt(&entry(), &range, None, "sys", 0);

    assert!(prompt.kept.len() < 20, "something was dropped");
    assert!(estimate_prompt_tokens(&entry(), "sys", &prompt.user) <= 2900);
    // the kept messages are the newest ones, in order
    let expected: Vec<Uuid> = range[20 - prompt.kept.len()..]
        .iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(
        prompt.kept.iter().map(|m| m.id).collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn the_first_drop_removes_ceil_of_a_fifth_of_the_messages() {
    let mut kept = long_range(20, 1);
    assert!(drop_oldest_fifth(&mut kept));
    assert_eq!(kept.len(), 16, "ceil(20 / 5) = 4");

    let mut seven = long_range(7, 1);
    assert!(drop_oldest_fifth(&mut seven));
    assert_eq!(seven.len(), 5, "ceil(7 / 5) = 2");

    let mut three = long_range(3, 1);
    assert!(drop_oldest_fifth(&mut three));
    assert_eq!(three.len(), 2, "keeping at least 2");
}

#[test]
fn at_most_two_messages_are_never_dropped_further() {
    let mut two = long_range(2, 1);
    assert!(!drop_oldest_fifth(&mut two));
    assert_eq!(two.len(), 2);
    let mut one = long_range(1, 1);
    assert!(!drop_oldest_fifth(&mut one));
}

#[test]
fn fitting_stops_at_two_messages_even_when_they_still_exceed_the_budget() {
    let range = long_range(6, 5_000);

    let prompt = build_prompt(&entry(), &range, None, "sys", 0);

    assert_eq!(prompt.kept.len(), 2);
    assert!(estimate_prompt_tokens(&entry(), "sys", &prompt.user) > 2900);
}

#[test]
fn the_stored_summary_counts_toward_the_budget() {
    let without = build_prompt(&entry(), &long_range(16, 90), None, "sys", 0);
    let with = build_prompt(
        &entry(),
        &long_range(16, 90),
        Some(&"s".repeat(1500)),
        "sys",
        0,
    );

    assert!(with.kept.len() <= without.kept.len());
    assert!(with.user.contains("<existing_summary>"));
}

#[test]
fn a_rebuild_after_a_retry_keeps_the_given_messages_and_the_system_prompt() {
    let kept = long_range(3, 10);

    let rebuilt = rebuild_prompt(kept.clone(), Some("old"), "sys", 0);

    assert_eq!(rebuilt.kept, kept);
    assert_eq!(rebuilt.system, "sys");
    assert_eq!(rebuilt.user, user_prompt(&kept, Some("old"), 0));
}
