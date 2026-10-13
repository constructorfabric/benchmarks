#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::sanitize_provider_message;

#[test]
fn provider_ids_urls_and_keys_are_replaced() {
    let raw = "Request resp_abc123def456 failed for file-AbCdEfGhIjKlMnOp at https://x.y/z using sk-ABCDEFGHIJKLMNOP now";

    assert_eq!(
        sanitize_provider_message(raw),
        "Request [provider_id] failed for [provider_id] at [url] using [credential] now"
    );
}

#[test]
fn bearer_tokens_become_a_credential_placeholder() {
    let sanitized =
        sanitize_provider_message("Rejected header Authorization: Bearer abcDEF123.xyz-token");

    assert!(sanitized.contains("[credential]"), "{sanitized}");
    assert!(!sanitized.contains("abcDEF123"), "{sanitized}");
}

#[test]
fn ordinary_words_that_look_like_prefixes_stay_intact() {
    let raw = "file_search failed on a file-based index near response";

    assert_eq!(sanitize_provider_message(raw), raw);
}

#[test]
fn completion_and_message_ids_are_recognized() {
    let sanitized = sanitize_provider_message(
        "chatcmpl-9aBcDeF cmpl-123 msg_0123456789abcdef vs_abcdefghijklmn",
    );

    assert_eq!(
        sanitized,
        "[provider_id] [provider_id] [provider_id] [provider_id]"
    );
}

#[test]
fn trailing_punctuation_is_not_part_of_the_url() {
    assert_eq!(
        sanitize_provider_message("see https://example.com/a, then retry."),
        "see [url], then retry."
    );
}

#[test]
fn very_long_messages_are_cut() {
    let sanitized = sanitize_provider_message(&"x".repeat(5000));

    assert_eq!(sanitized.chars().count(), 1024 + 3);
    assert!(sanitized.ends_with("..."));
}
