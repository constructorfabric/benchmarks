use super::*;

fn m(role: &str, content: &str) -> RangeMessage {
    RangeMessage { role: role.into(), content: content.into() }
}

#[test]
fn parse_extracts_summary_block_and_drops_analysis() {
    let out = parse_summary("<analysis>thinking</analysis>\n<summary>\nA\n\n\n\nB\n</summary>");
    assert_eq!(out, "A\n\nB");
}

#[test]
fn parse_without_summary_block_keeps_text_unless_markup_remains() {
    assert_eq!(parse_summary("<analysis>x</analysis> plain text"), "plain text");
    assert_eq!(parse_summary("<analysis>unterminated"), "");
    assert_eq!(parse_summary("   "), "");
}

#[test]
fn prompt_first_and_merge_forms() {
    let msgs = vec![m("user", "hello"), m("assistant", "hi there")];
    let p = user_prompt(None, &msgs, 0);
    assert!(p.starts_with(OPENING_FIRST));
    assert!(p.contains("User: hello\n\nAssistant: hi there"));
    assert!(p.ends_with(ANALYSIS_INSTRUCTION));
    let p = user_prompt(Some("old"), &msgs, 0);
    assert!(p.starts_with(OPENING_MERGE));
    assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
    assert!(p.contains("New messages to incorporate:"));
}

#[test]
fn prompt_truncates_long_content() {
    let p = user_prompt(None, &[m("user", "abcdefgh")], 3);
    assert!(p.contains("User: abc..."));
}

#[test]
fn token_estimate_uses_output_minus_reasoning_or_bytes() {
    let u = UsageTokens { output_tokens: 50, reasoning_tokens: 10, ..Default::default() };
    assert_eq!(token_estimate(Some(&u), "x"), 40);
    let u = UsageTokens { output_tokens: 10, reasoning_tokens: 10, ..Default::default() };
    assert_eq!(token_estimate(Some(&u), "abcde"), 2);
    assert_eq!(token_estimate(None, "abcdefgh"), 2);
}

#[test]
fn fitting_drops_oldest_but_keeps_two() {
    let msgs: Vec<RangeMessage> = (0..10).map(|i| m("user", &"x".repeat(400 + i))).collect();
    assert_eq!(fit_messages("s", None, &msgs, 0, None, 4), 0);
    let skip = fit_messages("s", None, &msgs, 0, Some(1), 4);
    assert_eq!(skip, 8);
    let big = fit_messages("s", None, &msgs, 0, Some(100_000), 4);
    assert_eq!(big, 0);
}
