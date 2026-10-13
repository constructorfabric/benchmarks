use super::*;

#[test]
fn response_id_is_replaced() {
    assert_eq!(
        sanitize_provider_message("failed for resp_abc123 today"),
        "failed for [provider_id] today"
    );
}

#[test]
fn completion_and_message_ids_are_replaced() {
    assert_eq!(
        sanitize_provider_message("chatcmpl-AbC123 cmpl-xyz9 msg_01Abc"),
        "[provider_id] [provider_id] [provider_id]"
    );
}

#[test]
fn ordinary_words_with_file_prefix_are_unchanged() {
    assert_eq!(sanitize_provider_message("file-based"), "file-based");
    assert_eq!(sanitize_provider_message("file_search"), "file_search");
    assert_eq!(
        sanitize_provider_message("uses file-based storage and file_search"),
        "uses file-based storage and file_search"
    );
}

#[test]
fn vector_store_id_is_replaced() {
    assert_eq!(
        sanitize_provider_message("store vs_abcdefghijkl missing"),
        "store [provider_id] missing"
    );
}

#[test]
fn file_and_assistant_ids_need_twelve_alphanumerics() {
    assert_eq!(
        sanitize_provider_message("file-abcdefghijkl"),
        "[provider_id]"
    );
    assert_eq!(
        sanitize_provider_message("file_abcdefghijkl"),
        "[provider_id]"
    );
    assert_eq!(
        sanitize_provider_message("assistant-abcdefghijkl"),
        "[provider_id]"
    );
    // eleven characters: below the floor
    assert_eq!(
        sanitize_provider_message("file-abcdefghijk"),
        "file-abcdefghijk"
    );
}

#[test]
fn url_is_replaced() {
    assert_eq!(sanitize_provider_message("https://x.y/z?a"), "[url]");
    assert_eq!(
        sanitize_provider_message("see http://example.com/a/b?c=d for details"),
        "see [url] for details"
    );
}

#[test]
fn secret_key_is_replaced() {
    assert_eq!(sanitize_provider_message("sk-ABCDEFGHIJKL"), "[credential]");
    // nine characters: below the floor
    assert_eq!(sanitize_provider_message("sk-ABCDEFGHI"), "sk-ABCDEFGHI");
}

#[test]
fn bearer_token_is_replaced_keeping_the_word_bearer() {
    assert_eq!(
        sanitize_provider_message("Bearer eyJabc.def"),
        "Bearer [credential]"
    );
    assert_eq!(
        sanitize_provider_message("header Authorization: Bearer abc-123_x+y/z= rejected"),
        "header Authorization: Bearer [credential] rejected"
    );
}

#[test]
fn message_without_identifiers_is_unchanged() {
    let msg = "The model is overloaded, please try again later.";
    assert_eq!(sanitize_provider_message(msg), msg);
}

#[test]
fn several_kinds_in_one_message() {
    assert_eq!(
        sanitize_provider_message("resp_1 at https://api.example.com/v1 with sk-ABCDEFGHIJKL"),
        "[provider_id] at [url] with [credential]"
    );
}
