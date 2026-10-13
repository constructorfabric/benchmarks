#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn the_summary_block_wins_over_the_analysis() {
    let answer = "<analysis>reasoning</analysis>\n<summary>\nTHE SUMMARY\n</summary>";

    assert_eq!(parse_summary(answer).as_deref(), Some("THE SUMMARY"));
}

#[test]
fn text_around_the_blocks_is_ignored() {
    let answer = "preface <analysis>a</analysis> middle <summary>INSIDE</summary> trailing";

    assert_eq!(parse_summary(answer).as_deref(), Some("INSIDE"));
}

#[test]
fn blank_line_runs_collapse_to_one_blank_line() {
    assert_eq!(
        parse_summary("<summary>one\n\n\n\ntwo\n  \n\nthree</summary>").as_deref(),
        Some("one\n\ntwo\n\nthree")
    );
    assert_eq!(
        parse_summary("Just text\n\n\n\nmore text").as_deref(),
        Some("Just text\n\nmore text")
    );
}

#[test]
fn without_a_summary_block_the_remaining_text_is_the_summary() {
    assert_eq!(
        parse_summary("<analysis>x</analysis>\nPlain summary text").as_deref(),
        Some("Plain summary text")
    );
}

#[test]
fn leftover_markup_or_blank_results_are_empty() {
    for answer in [
        "<summary>never closed",
        "<analysis>never closed analysis",
        "text with a stray <summary tag",
        "",
        "   \n\n ",
        "<summary>   </summary>",
        "<analysis>only reasoning</analysis>",
    ] {
        assert_eq!(parse_summary(answer), None, "`{answer}`");
    }
}

#[test]
fn the_token_estimate_prefers_the_reported_output_without_reasoning() {
    let usage = TokenUsage {
        input_tokens: 500,
        output_tokens: 200,
        reasoning_tokens: 50,
        ..TokenUsage::default()
    };

    assert_eq!(token_estimate("ABCDEFGHIJKLMNOPQRSTU", Some(&usage)), 150);
}

#[test]
fn the_token_estimate_falls_back_to_a_quarter_of_the_bytes() {
    let only_reasoning = TokenUsage {
        output_tokens: 40,
        reasoning_tokens: 40,
        ..TokenUsage::default()
    };

    assert_eq!(token_estimate("ABCDEFGHIJKLMNOPQRSTU", None), 6);
    assert_eq!(
        token_estimate("ABCDEFGHIJKLMNOPQRSTU", Some(&only_reasoning)),
        6
    );
    assert_eq!(
        token_estimate("ABCDEFGHIJKLMNOPQRSTU", Some(&TokenUsage::default())),
        6
    );
}
