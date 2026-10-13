#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn replaces_response_ids() {
    assert_eq!(
        sanitize_provider_message("bad resp_abc123 x"),
        "bad [provider_id] x"
    );
    assert_eq!(
        sanitize_provider_message("chatcmpl-AbC9 cmpl-xyz msg_01ABC"),
        "[provider_id] [provider_id] [provider_id]"
    );
}

#[test]
fn file_ids_need_12_chars() {
    assert_eq!(
        sanitize_provider_message("file-abc123def456 missing"),
        "[provider_id] missing"
    );
    assert_eq!(
        sanitize_provider_message("file_abc123def456"),
        "[provider_id]"
    );
    assert_eq!(
        sanitize_provider_message("file-based file_search"),
        "file-based file_search"
    );
}

#[test]
fn vs_and_assistant_ids() {
    assert_eq!(
        sanitize_provider_message("vs_abc123def456 and assistant-ABCDEFGHIJKL1"),
        "[provider_id] and [provider_id]"
    );
    assert_eq!(sanitize_provider_message("vs_short"), "vs_short");
}

#[test]
fn urls_replaced() {
    assert_eq!(
        sanitize_provider_message("see https://api.openai.com/v1/x?y=1 now"),
        "see [url] now"
    );
    assert_eq!(sanitize_provider_message("http://h/resp_abc"), "[url]");
}

#[test]
fn sk_keys_and_bearer_tokens_replaced() {
    assert_eq!(
        sanitize_provider_message("key sk-abcdef1234567890 bad"),
        "key [credential] bad"
    );
    assert_eq!(
        sanitize_provider_message("Authorization: Bearer abc.def-ghi"),
        "Authorization: [credential]"
    );
    assert_eq!(sanitize_provider_message("Bearer sk-abcdefghij"), "[credential]");
    assert_eq!(sanitize_provider_message("sk-short"), "sk-short");
}

#[test]
fn plain_text_untouched() {
    assert_eq!(
        sanitize_provider_message("Rate limit reached, try later."),
        "Rate limit reached, try later."
    );
}

#[test]
fn user_field_is_64_hex() {
    let t = "11111111-2222-3333-4444-555555555555";
    let u = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let f = user_field(t, u);
    assert_eq!(f.len(), 64);
    assert_eq!(f, "11111111222233334444555555555555aaaaaaaabbbbccccddddeeeeeeeeeeee");
}

#[test]
fn user_field_falls_back_for_non_uuid() {
    assert_eq!(user_field("tenant", "user"), "tenant:user");
    assert_eq!(
        user_field("11111111-2222-3333-4444-555555555555", "bob"),
        "11111111-2222-3333-4444-555555555555:bob"
    );
}
