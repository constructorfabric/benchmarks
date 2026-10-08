use uuid::Uuid;

use super::*;

#[test]
fn file_id_is_replaced() {
    assert_eq!(
        sanitize_provider_message("file-abc123def456ghi not found"),
        "[provider_id] not found"
    );
}

#[test]
fn vector_store_id_is_replaced() {
    assert_eq!(
        sanitize_provider_message("vs_abcdefghijklmn"),
        "[provider_id]"
    );
}

#[test]
fn ordinary_words_are_kept() {
    assert_eq!(sanitize_provider_message("file-based"), "file-based");
    assert_eq!(sanitize_provider_message("file_search"), "file_search");
    assert_eq!(
        sanitize_provider_message("the file_search tool failed on a file-based input"),
        "the file_search tool failed on a file-based input"
    );
}

#[test]
fn url_is_replaced() {
    assert_eq!(sanitize_provider_message("see https://x.y/z"), "see [url]");
    assert_eq!(
        sanitize_provider_message("docs at http://example.com/a?b=c, then retry"),
        "docs at [url], then retry"
    );
}

#[test]
fn credentials_are_replaced() {
    assert_eq!(sanitize_provider_message("sk-ABCDEFGHIJ12"), "[credential]");
    assert_eq!(sanitize_provider_message("Bearer abc.def"), "[credential]");
    assert_eq!(
        sanitize_provider_message("Incorrect API key provided: sk-proj-ABCDEFGHIJKL."),
        "Incorrect API key provided: [credential]."
    );
    // Too short to be a key.
    assert_eq!(sanitize_provider_message("sk-short"), "sk-short");
}

#[test]
fn response_ids_are_replaced() {
    assert_eq!(sanitize_provider_message("resp_123abc"), "[provider_id]");
    assert_eq!(
        sanitize_provider_message("chatcmpl-9x8Y and cmpl-1a and msg_01AbC failed"),
        "[provider_id] and [provider_id] and [provider_id] failed"
    );
    assert_eq!(
        sanitize_provider_message("assistant-ABCDEFGHIJKLMN file_abcdefghijkl"),
        "[provider_id] [provider_id]"
    );
}

#[test]
fn other_text_is_left_as_is() {
    let msg = "The server had an error while processing your request.";
    assert_eq!(sanitize_provider_message(msg), msg);
}

#[test]
fn provider_user_field_is_64_lowercase_hex_tenant_first() {
    let tenant = Uuid::parse_str("A1B2C3D4-0000-4000-8000-000000000001").unwrap();
    let user = Uuid::parse_str("11111111-6a88-4768-9dfc-6bcd5187d9ed").unwrap();
    let v = provider_user_field(tenant, user);
    assert_eq!(v.len(), 64);
    assert!(
        v.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_eq!(
        v,
        "a1b2c3d4000040008000000000000001111111116a8847689dfc6bcd5187d9ed"
    );
}
