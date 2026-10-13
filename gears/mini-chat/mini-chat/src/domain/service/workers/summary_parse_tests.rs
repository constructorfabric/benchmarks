#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::non_ascii_literal
)]
//! Slice 5: parsing of the summary answer and the `token_estimate` rule
//! (`inst-parse-summary`, `inst-if-summary-empty`, `inst-token-estimate`).

use mini_chat_sdk::UsageTokens;

use super::{parse_summary, token_estimate};

fn usage(output: u64, reasoning: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: 100,
        output_tokens: output,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: reasoning,
    }
}

#[test]
fn the_summary_block_is_kept_and_the_analysis_block_is_dropped() {
    let answer = "<analysis>my reasoning</analysis>\n<summary>\nThe user wants X.\n</summary>";

    assert_eq!(parse_summary(answer).as_deref(), Some("The user wants X."));
}

#[test]
fn the_analysis_inside_the_summary_text_is_removed_first() {
    let answer = "<summary>Before <analysis>hidden</analysis>after</summary>";

    assert_eq!(parse_summary(answer).as_deref(), Some("Before after"));
}

#[test]
fn every_analysis_block_is_removed() {
    let answer = "<analysis>a</analysis>one<analysis>b</analysis> two";

    assert_eq!(parse_summary(answer).as_deref(), Some("one two"));
}

#[test]
fn a_plain_answer_without_tags_is_kept() {
    assert_eq!(
        parse_summary("Just a summary.").as_deref(),
        Some("Just a summary.")
    );
}

#[test]
fn an_answer_with_an_unclosed_analysis_tag_is_empty() {
    assert_eq!(
        parse_summary("<analysis>never closed ... the summary"),
        None
    );
}

#[test]
fn an_unclosed_summary_tag_is_empty() {
    assert_eq!(parse_summary("<summary>never closed"), None);
}

#[test]
fn a_summary_tag_without_a_block_is_empty() {
    assert_eq!(parse_summary("text </summary> and <summary"), None);
}

#[test]
fn an_empty_or_whitespace_result_is_none() {
    assert_eq!(parse_summary(""), None);
    assert_eq!(parse_summary("   \n \n"), None);
    assert_eq!(parse_summary("<analysis>only analysis</analysis>"), None);
    assert_eq!(parse_summary("<summary>  \n </summary>"), None);
}

#[test]
fn runs_of_blank_lines_are_collapsed_and_the_result_is_trimmed() {
    let answer = "<summary>\n\n  First paragraph\n\n\n\n   \nSecond paragraph\n\n</summary>";

    assert_eq!(
        parse_summary(answer).as_deref(),
        Some("First paragraph\n\nSecond paragraph")
    );
}

#[test]
fn the_text_before_the_first_summary_block_is_ignored_when_a_block_exists() {
    let answer = "Sure! <summary>The summary</summary> Hope that helps.";

    assert_eq!(parse_summary(answer).as_deref(), Some("The summary"));
}

#[test]
fn only_the_first_summary_block_counts() {
    assert_eq!(
        parse_summary("<summary>one</summary><summary>two</summary>").as_deref(),
        Some("one")
    );
}

#[test]
fn the_token_estimate_is_output_minus_reasoning_when_positive() {
    assert_eq!(token_estimate(&usage(120, 20), "ignored text"), 100);
    assert_eq!(token_estimate(&usage(50, 0), "ignored text"), 50);
}

#[test]
fn without_a_positive_difference_it_is_the_byte_length_divided_by_four_rounded_up() {
    assert_eq!(token_estimate(&usage(0, 0), "abcdefgh"), 2);
    assert_eq!(
        token_estimate(&usage(0, 0), "abcdefghi"),
        3,
        "9 bytes round up"
    );
    assert_eq!(
        token_estimate(&usage(10, 10), "abcd"),
        1,
        "reasoning equal to the output"
    );
    assert_eq!(
        token_estimate(&usage(10, 30), "abcde"),
        2,
        "reasoning above the output"
    );
    assert_eq!(token_estimate(&usage(0, 0), "é"), 1, "two bytes");
    assert_eq!(token_estimate(&usage(0, 0), "日本語"), 3, "nine bytes");
}

#[test]
fn an_empty_text_without_usage_estimates_zero() {
    assert_eq!(token_estimate(&usage(0, 0), ""), 0);
}
