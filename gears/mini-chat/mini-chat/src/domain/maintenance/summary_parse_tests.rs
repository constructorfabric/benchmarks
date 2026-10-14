//! The summary response parsing: the `<analysis>` block removed, the `<summary>` content taken,
//! the whole remaining text taken without a `<summary>` block unless it still holds markup,
//! blank-line runs collapsed and the result trimmed, an empty result, the `token_estimate` rule
//! (output minus reasoning tokens when positive, else the rounded-up quarter of the byte length)
//! and the absence of any length or entropy check.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing` (`inst-rp-strip-analysis`,
//! `inst-rp-summary-block`, `inst-rp-take-summary`, `inst-rp-leftover-markup`,
//! `inst-rp-empty-markup`, `inst-rp-take-rest`, `inst-rp-collapse`, `inst-rp-empty`,
//! `inst-rp-return-empty`, `inst-rp-token-estimate`, `inst-rp-no-quality-gate`, `inst-rp-return`);
//! definition of done `cpt-cf-mini-chat-dod-background-maintenance-summary-execution`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::non_ascii_literal,
    reason = "multi-byte content is the subject of these tests"
)]

use mini_chat_sdk::UsageTokens;

use super::{ParsedSummary, parse_summary_text};

fn usage(output: u64, reasoning: u64) -> UsageTokens {
    UsageTokens {
        output_tokens: output,
        reasoning_tokens: reasoning,
        ..UsageTokens::default()
    }
}

fn parsed(text: &str) -> Option<ParsedSummary> {
    parse_summary_text(text, &usage(50, 0))
}

fn text_of(text: &str) -> String {
    parsed(text)
        .unwrap_or_else(|| panic!("`{text}` parses to a summary"))
        .text
}

#[test]
fn the_analysis_block_is_removed_and_the_summary_content_is_taken() {
    // inst-rp-strip-analysis, inst-rp-summary-block, inst-rp-take-summary
    assert_eq!(
        text_of("<analysis>my reasoning</analysis>\n<summary>The summary.</summary>"),
        "The summary."
    );
}

#[test]
fn a_multiline_analysis_block_is_removed_whole() {
    // inst-rp-strip-analysis
    let text = "<analysis>\nstep 1\nstep 2\n</analysis>\n\n<summary>\nS1\nS2\n</summary>";
    assert_eq!(text_of(text), "S1\nS2");
}

#[test]
fn every_analysis_block_is_removed() {
    // inst-rp-strip-analysis: "every `<analysis>...</analysis>` block"
    let text = "<analysis>one</analysis>text<analysis>two</analysis><summary>S</summary>";
    assert_eq!(text_of(text), "S");
}

#[test]
fn a_summary_tag_inside_the_analysis_does_not_count() {
    // inst-rp-strip-analysis: the analysis goes first, so a tag inside it is gone with it
    let text = "<analysis>I will write a <summary> block soon</analysis><summary>real</summary>";
    assert_eq!(text_of(text), "real");
}

#[test]
fn text_around_the_summary_block_is_dropped() {
    // inst-rp-take-summary: only the text inside the block is stored
    assert_eq!(
        text_of("preamble <summary>kept</summary> postscript"),
        "kept"
    );
}

#[test]
fn without_a_summary_block_the_whole_remaining_text_is_taken() {
    // inst-rp-take-rest
    assert_eq!(
        text_of("Just a plain summary text."),
        "Just a plain summary text."
    );
    assert_eq!(
        text_of("<analysis>gone</analysis>The rest is the summary."),
        "The rest is the summary."
    );
}

#[test]
fn leftover_analysis_markup_gives_an_empty_result() {
    // inst-rp-leftover-markup, inst-rp-empty-markup: an unclosed `<analysis`
    assert_eq!(
        parsed("<analysis>never closed and no summary follows"),
        None
    );
    assert_eq!(parsed("text <analysis attr=\"x\">open"), None);
}

#[test]
fn leftover_summary_markup_gives_an_empty_result() {
    // inst-rp-leftover-markup: a truncated completion misses the closing `</summary>`
    assert_eq!(parsed("<summary>text that was cut off"), None);
    assert_eq!(parsed("<analysis>a</analysis><summary>cut"), None);
    assert_eq!(parsed("stray </summary> closer and <summary"), None);
}

#[test]
fn blank_line_runs_are_collapsed_into_one_and_the_result_is_trimmed() {
    // inst-rp-collapse
    assert_eq!(
        text_of("<summary>\n\n  First paragraph.\n\n\n\nSecond paragraph.\n\n\n</summary>"),
        "First paragraph.\n\nSecond paragraph."
    );
    assert_eq!(text_of("a\n\n\n\n\nb"), "a\n\nb");
    assert_eq!(text_of("line one\nline two"), "line one\nline two");
}

#[test]
fn an_empty_result_is_none() {
    // inst-rp-empty, inst-rp-return-empty
    for text in [
        "",
        "   \n\t ",
        "<summary></summary>",
        "<summary>   \n  </summary>",
        "<analysis>only analysis</analysis>",
        "<analysis>a</analysis>\n\n<analysis>b</analysis>",
    ] {
        assert_eq!(parsed(text), None, "`{text}`");
    }
}

#[test]
fn the_estimate_is_output_minus_reasoning_when_positive() {
    // inst-rp-token-estimate
    let result = parse_summary_text("<summary>some text</summary>", &usage(500, 120)).unwrap();
    assert_eq!(result.token_estimate, 380);
    let plain = parse_summary_text("some text", &usage(77, 0)).unwrap();
    assert_eq!(plain.token_estimate, 77);
}

#[test]
fn a_zero_difference_falls_back_to_the_rounded_up_quarter_of_the_bytes() {
    // inst-rp-token-estimate: output equals reasoning
    let result = parse_summary_text("<summary>abcdefghi</summary>", &usage(100, 100)).unwrap();
    assert_eq!(result.text, "abcdefghi");
    assert_eq!(result.token_estimate, 3, "ceil(9 / 4)");
}

#[test]
fn reasoning_above_output_falls_back_to_the_byte_estimate() {
    // inst-rp-token-estimate: the difference is not positive
    let result = parse_summary_text("abcdefgh", &usage(10, 40)).unwrap();
    assert_eq!(result.token_estimate, 2, "ceil(8 / 4)");
}

#[test]
fn a_zero_output_count_falls_back_to_the_byte_estimate() {
    // inst-rp-token-estimate: the provider reported 0
    let result = parse_summary_text("abcdefghijklm", &usage(0, 0)).unwrap();
    assert_eq!(result.token_estimate, 4, "ceil(13 / 4)");
    let exact = parse_summary_text("abcdefgh", &UsageTokens::default()).unwrap();
    assert_eq!(exact.token_estimate, 2);
}

#[test]
fn the_byte_estimate_counts_utf8_bytes_of_the_stored_text() {
    // inst-rp-token-estimate: bytes, not characters, of the summary after parsing
    let result = parse_summary_text("<summary>ééé</summary>", &usage(0, 0)).unwrap();
    assert_eq!(result.text, "ééé");
    assert_eq!(result.token_estimate, 2, "ceil(6 / 4)");
}

#[test]
fn the_estimate_is_taken_from_the_call_not_from_the_removed_analysis() {
    // inst-rp-token-estimate: the provider's output count still includes the removed analysis
    let long_analysis = format!(
        "<analysis>{}</analysis><summary>x</summary>",
        "a".repeat(4000)
    );
    let result = parse_summary_text(&long_analysis, &usage(1200, 0)).unwrap();
    assert_eq!(result.token_estimate, 1200);
}

#[test]
fn no_length_or_entropy_check_is_applied() {
    // inst-rp-no-quality-gate: a one-character summary, a repetitive one and a huge one pass
    assert_eq!(text_of("x"), "x");
    let repetitive = "ab".repeat(50);
    assert_eq!(text_of(&repetitive), repetitive);
    let huge = "w".repeat(200_000);
    let result = parse_summary_text(&format!("<summary>{huge}</summary>"), &usage(10, 0)).unwrap();
    assert_eq!(result.text.len(), 200_000);
}

#[test]
fn the_result_holds_the_text_and_the_estimate() {
    // inst-rp-return
    assert_eq!(
        parse_summary_text("<summary>S</summary>", &usage(9, 4)),
        Some(ParsedSummary {
            text: "S".to_owned(),
            token_estimate: 5
        })
    );
}
