use mini_chat_sdk::UsageTokens;
use time::OffsetDateTime;
use uuid::Uuid;

use super::*;
use crate::domain::context::HistoryMessage;
use crate::infra::llm::types::Role;

fn msg(role: Role, content: &str) -> HistoryMessage {
    HistoryMessage {
        id: Uuid::new_v4(),
        role,
        content: content.to_owned(),
        created_at: OffsetDateTime::UNIX_EPOCH,
    }
}

fn conversation() -> Vec<HistoryMessage> {
    vec![msg(Role::User, "hi"), msg(Role::Assistant, "hello there")]
}

#[test]
fn prompt_first_summary_starts_with_summarize_line() {
    let prompt = build_summary_prompt(None, &conversation(), 4000);
    assert!(
        prompt.starts_with(
            "Summarize the following conversation:\n\nUser: hi\n\nAssistant: hello there\n\n"
        ),
        "{prompt}"
    );
    assert!(prompt.ends_with(ANALYSIS_INSTRUCTION), "{prompt}");
    assert!(!prompt.contains("<existing_summary>"), "{prompt}");
    assert!(
        ANALYSIS_INSTRUCTION.starts_with(
            "Before providing your final summary, wrap your analysis in <analysis> tags."
        ) && ANALYSIS_INSTRUCTION
            .ends_with("Respond with an <analysis> block followed by a <summary> block.")
    );
}

#[test]
fn prompt_with_existing_summary_block() {
    let prompt = build_summary_prompt(Some("OLD SUMMARY"), &conversation(), 4000);
    let expected_head = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\n\
IMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.\n\n\
<existing_summary>\nOLD SUMMARY\n</existing_summary>\n\n\
New messages to incorporate:\n\n\
User: hi\n\nAssistant: hello there\n\n";
    assert!(prompt.starts_with(expected_head), "{prompt}");
    assert!(prompt.ends_with(ANALYSIS_INSTRUCTION), "{prompt}");
    assert!(!prompt.contains("Summarize the following conversation:"));
}

#[test]
fn content_truncated_with_ellipsis() {
    let msgs = vec![
        msg(Role::User, "abcdefgh"),
        msg(Role::Assistant, "abcde"),
        msg(Role::User, "\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"),
    ];
    let prompt = build_summary_prompt(None, &msgs, 5);
    assert!(prompt.contains("User: abcde...\n\n"), "{prompt}");
    assert!(prompt.contains("Assistant: abcde\n\n"), "{prompt}");
    // Characters, not bytes.
    assert!(
        prompt.contains("User: \u{e9}\u{e9}\u{e9}\u{e9}\u{e9}...\n\n"),
        "{prompt}"
    );

    let unlimited = build_summary_prompt(None, &msgs, 0);
    assert!(unlimited.contains("User: abcdefgh\n\n"), "{unlimited}");
}

#[test]
fn parse_extracts_summary_block() {
    let text = "<analysis>\nthinking <b>hard</b>\n</analysis>\n\n<summary>\n1. Purpose: A\n\n\n\n2. Info: B\n  \n\n3. Done\n</summary>\n";
    assert_eq!(
        parse_summary(text),
        "1. Purpose: A\n\n2. Info: B\n\n3. Done"
    );
    assert_eq!(
        parse_summary("<analysis>x</analysis><summary>S</summary>"),
        "S"
    );
    // No <summary> block: the remaining text without the analysis.
    assert_eq!(
        parse_summary("<analysis>x</analysis>\nPlain summary\n\n\n\nline 2"),
        "Plain summary\n\nline 2"
    );
}

#[test]
fn parse_without_tags_containing_markup_is_empty() {
    assert_eq!(parse_summary("<analysis>never closed, no summary"), "");
    assert_eq!(parse_summary("<summary>never closed"), "");
    assert_eq!(parse_summary("<analysis>a</analysis>"), "");
    assert_eq!(parse_summary("   \n\n "), "");
    assert_eq!(parse_summary("just text"), "just text");
}

#[test]
fn token_estimate_fallback_bytes_div_4() {
    let usage = |output_tokens, reasoning_tokens| UsageTokens {
        input_tokens: 100,
        output_tokens,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens,
    };
    // 9 bytes -> ceil(9 / 4) = 3.
    assert_eq!(summary_token_estimate(None, "123456789"), 3);
    assert_eq!(summary_token_estimate(Some(&usage(0, 0)), "12345678"), 2);
    assert_eq!(summary_token_estimate(Some(&usage(10, 10)), "12345"), 2);
    assert_eq!(summary_token_estimate(Some(&usage(20, 5)), "12345"), 15);
}

#[test]
fn fit_drops_oldest_fifth_keeping_two() {
    let msgs: Vec<HistoryMessage> = (0..10)
        .map(|i| msg(Role::User, &format!("message {i} {}", "x".repeat(400))))
        .collect();
    // No budget: everything is kept.
    let all = fit_prompt("sys", None, &msgs, 4000, None, 4);
    assert_eq!(all, build_summary_prompt(None, &msgs, 4000));

    // A budget that fits about 5 messages: ceil(10/5) = 2 dropped per step.
    let five = build_summary_prompt(None, &msgs[5..], 4000);
    let budget = i64::try_from(("sys".len() + five.len()).div_ceil(4)).unwrap();
    let fitted = fit_prompt("sys", None, &msgs, 4000, Some(budget), 4);
    assert!(!fitted.contains("message 3 "), "oldest dropped");
    assert!(fitted.contains("message 9 "));
    assert!(("sys".len() + fitted.len()).div_ceil(4) <= usize::try_from(budget).unwrap());
    assert_eq!(fitted, build_summary_prompt(None, &msgs[6..], 4000));

    // An impossible budget keeps the newest two messages.
    let two = fit_prompt("sys", None, &msgs, 4000, Some(1), 4);
    assert_eq!(two, build_summary_prompt(None, &msgs[8..], 4000));
}
