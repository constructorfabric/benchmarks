use super::*;

fn model() -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": "sum",
        "provider_model_id": "sum-p",
        "display_name": "Sum",
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "enabled": true,
        "context_window": 2_000,
        "max_output_tokens": 500,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 1_000_000,
        "max_num_results": 5,
        "general_config": {}
    }))
    .expect("entry")
}

fn msgs(n: usize, size: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| {
            let role = if i % 2 == 0 { "user" } else { "assistant" };
            (role.to_owned(), format!("m{i:02} {}", "x".repeat(size)))
        })
        .collect()
}

#[test]
fn system_prompt_precedence() {
    let mut m = model();
    assert_eq!(system_prompt(&m, ""), DEFAULT_SUMMARY_SYSTEM_PROMPT);
    assert_eq!(system_prompt(&m, "configured"), "configured");
    m.thread_summary_prompt = "catalog".to_owned();
    assert_eq!(system_prompt(&m, "configured"), "catalog");
}

#[test]
fn user_prompt_without_existing_summary() {
    let p = user_prompt(
        None,
        &[
            ("user".into(), "hi".into()),
            ("assistant".into(), "hello".into()),
        ],
        4000,
    );
    assert!(
        p.starts_with("Summarize the following conversation:\n\nUser: hi\n\nAssistant: hello\n\n")
    );
    assert!(p.ends_with("Respond with an <analysis> block followed by a <summary> block."));
    assert!(!p.contains("<existing_summary>"));
}

#[test]
fn user_prompt_with_existing_summary_and_cut() {
    let p = user_prompt(
        Some("old facts"),
        &[("user".into(), "abcdefghij".into())],
        4,
    );
    assert!(p.starts_with("The existing summary below covers the earlier conversation."));
    assert!(p.contains("<existing_summary>\nold facts\n</existing_summary>"));
    assert!(p.contains("New messages to incorporate:\n\nUser: abcd..."));
    // 0 = no limit
    let p = user_prompt(None, &[("user".into(), "abcdefghij".into())], 0);
    assert!(p.contains("User: abcdefghij\n"));
}

#[test]
fn parse_summary_variants() {
    assert_eq!(
        parse_summary(
            "<analysis>thinking\nmore</analysis>\n<summary>\nLine 1\n\n\n\nLine 2\n</summary>"
        ),
        "Line 1\n\nLine 2"
    );
    assert_eq!(
        parse_summary("No tags at all.\n\n\nSecond."),
        "No tags at all.\n\nSecond."
    );
    assert_eq!(parse_summary("<analysis>only</analysis>"), "");
    assert_eq!(parse_summary("<analysis>unterminated"), "");
    assert_eq!(parse_summary("<summary>open ended"), "open ended");
    assert_eq!(parse_summary("text with <summary-ish markup"), "");
}

#[test]
fn fit_drops_oldest_fifth_keeping_two() {
    let m = model();
    // budget = 2000 - 500 = 1500 tokens at 4 bytes/token
    let fitted = fit_messages(&m, "sys", None, msgs(10, 1_000), 4000);
    assert!(fitted.len() >= 2);
    assert!(fitted.len() < 10);
    assert_eq!(fitted.last().unwrap().1, msgs(10, 1_000).last().unwrap().1);
    let prompt = user_prompt(None, &fitted, 4000);
    #[allow(clippy::integer_division)] // floor estimate at 4 bytes/token, as in the budget comment
    let approx_tokens = prompt.len() / 4;
    assert!(approx_tokens <= 1_500 || fitted.len() == 2);
    // never below two messages
    let fitted = fit_messages(&m, "sys", None, msgs(3, 50_000), 0);
    assert_eq!(fitted.len(), 2);
}

#[test]
fn no_fitting_without_context_window() {
    let mut m = model();
    m.context_window = 0;
    assert_eq!(fit_messages(&m, "sys", None, msgs(10, 10_000), 0).len(), 10);
}
