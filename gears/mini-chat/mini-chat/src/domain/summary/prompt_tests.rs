#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::integer_division,
    reason = "the prompt size is bytes / bytes_per_token_conservative (DESIGN 3.6)"
)]

use super::*;

fn message(role: InputRole, content: &str) -> RangeMessage {
    RangeMessage {
        role,
        content: content.to_owned(),
    }
}

fn conversation(count: usize, bytes: usize) -> Vec<RangeMessage> {
    (0..count)
        .map(|index| {
            let role = if index % 2 == 0 {
                InputRole::User
            } else {
                InputRole::Assistant
            };
            message(role, &"x".repeat(bytes))
        })
        .collect()
}

#[test]
fn a_first_summary_prompt_opens_with_the_instruction_and_ends_with_the_analysis_request() {
    let prompt = user_prompt(
        None,
        &[
            message(InputRole::User, "hi"),
            message(InputRole::Assistant, "hello"),
        ],
        0,
    );

    assert!(
        prompt.starts_with(
            "Summarize the following conversation:\n\nUser: hi\n\nAssistant: hello\n\n"
        )
    );
    assert!(prompt.ends_with("Respond with an <analysis> block followed by a <summary> block."));
    assert!(!prompt.contains("<existing_summary>"));
}

#[test]
fn an_update_prompt_embeds_the_existing_summary_before_the_new_messages() {
    let prompt = user_prompt(Some("OLD"), &[message(InputRole::User, "new")], 0);

    let existing = prompt
        .find("<existing_summary>\nOLD\n</existing_summary>")
        .unwrap();
    let marker = prompt
        .find("New messages to incorporate:\n\nUser: new")
        .unwrap();
    assert!(existing < marker);
    assert!(prompt.starts_with("The existing summary below covers the earlier conversation."));
}

#[test]
fn content_over_the_limit_is_cut_and_marked() {
    let long = message(InputRole::User, "0123456789ABCDEF");

    assert_eq!(entry(&long, 10), "User: 0123456789...");
    assert_eq!(entry(&long, 16), "User: 0123456789ABCDEF");
    assert_eq!(entry(&long, 0), "User: 0123456789ABCDEF");
}

#[test]
fn the_limit_counts_characters_not_bytes() {
    let text = message(InputRole::Assistant, "\u{e4}\u{e4}\u{e4}\u{e4}");

    assert_eq!(entry(&text, 3), "Assistant: \u{e4}\u{e4}\u{e4}...");
}

#[test]
fn the_drop_step_is_a_fifth_and_keeps_two() {
    let steps: Vec<usize> = [10, 8, 6, 4, 3, 2, 1, 0]
        .iter()
        .map(|count| drop_step(*count))
        .collect();

    assert_eq!(steps, [2, 2, 2, 1, 1, 0, 0, 0]);
}

#[test]
fn the_budget_is_the_window_minus_the_output_capped_by_max_input() {
    assert_eq!(input_budget(2000, 500, 0), Some(1500));
    assert_eq!(input_budget(128_000, 500, 1500), Some(1500));
    assert_eq!(input_budget(0, 500, 1500), None);
    assert_eq!(input_budget(1000, 900, 0), Some(100));
}

#[test]
fn fitting_drops_the_oldest_messages_step_by_step_until_the_prompt_fits() {
    let messages = conversation(10, 600);
    let tokens =
        |dropped: usize| ("system".len() + user_prompt(None, &messages[dropped..], 0).len()) / 4;

    let dropped = fit("system", None, &messages, 0, Some(1500), 4);

    assert!(dropped > 0, "the whole conversation does not fit");
    assert!(tokens(dropped) <= 1500);
    // `dropped` is a sum of whole steps (10 -> 8 -> 6 ...) and the step before did not fit.
    let (mut before, mut at) = (0, 0);
    while at < dropped {
        before = at;
        at += drop_step(messages.len() - at);
    }
    assert_eq!(at, dropped);
    assert!(tokens(before) > 1500);
}

#[test]
fn fitting_keeps_two_messages_when_nothing_fits() {
    let messages = conversation(6, 600);

    assert_eq!(fit("system", None, &messages, 0, Some(1), 4), 4);
}

#[test]
fn no_budget_means_no_fitting() {
    let messages = conversation(10, 600);

    assert_eq!(fit("system", None, &messages, 0, None, 4), 0);
}
