use super::sanitize_provider_message;

#[test]
fn replaces_provider_ids() {
    assert_eq!(
        sanitize_provider_message("response resp_abc123 failed"),
        "response [provider_id] failed"
    );
    assert_eq!(sanitize_provider_message("id chatcmpl-9xYz"), "id [provider_id]");
    assert_eq!(sanitize_provider_message("id cmpl-77a"), "id [provider_id]");
    assert_eq!(sanitize_provider_message("msg_01ABCdef bad"), "[provider_id] bad");
    assert_eq!(
        sanitize_provider_message("File file-AbCdEf123456789 not found"),
        "File [provider_id] not found"
    );
    assert_eq!(
        sanitize_provider_message("vector store vs_abcdefghijkl12 missing; assistant-ABCDEFGHIJKL1"),
        "vector store [provider_id] missing; [provider_id]"
    );
    assert_eq!(sanitize_provider_message("file_abcdefabcdef1"), "[provider_id]");
}

#[test]
fn keeps_ordinary_words() {
    let s = "file-based retrieval uses file_search and vs_short ids";
    assert_eq!(sanitize_provider_message(s), s);
}

#[test]
fn replaces_urls_and_credentials() {
    assert_eq!(
        sanitize_provider_message("see https://api.example.com/v1/files?x=1 now"),
        "see [url] now"
    );
    assert_eq!(
        sanitize_provider_message("invalid key sk-abcdefghij1234"),
        "invalid key [credential]"
    );
    assert_eq!(sanitize_provider_message("sk-short"), "sk-short");
    assert_eq!(
        sanitize_provider_message("header Bearer eyJhbGciOiJIUzI1NiJ9.x.y rejected"),
        "header [credential] rejected"
    );
}
