use super::sanitize_provider_message as s;

#[test]
fn replaces_response_and_completion_ids() {
    assert_eq!(s("failed resp_abc123 now"), "failed [provider_id] now");
    assert_eq!(
        s("id chatcmpl-9xYz1 and cmpl-77"),
        "id [provider_id] and [provider_id]"
    );
    assert_eq!(s("msg_01ABC failed"), "[provider_id] failed");
}

#[test]
fn replaces_file_and_vector_store_ids_with_length_floor() {
    assert_eq!(s("file-abcdefghijkl1 missing"), "[provider_id] missing");
    assert_eq!(s("vs_abcdefghijkl12"), "[provider_id]");
    assert_eq!(s("assistant-ABCDEFGHIJKLM"), "[provider_id]");
    assert_eq!(s("file_0123456789ab"), "[provider_id]");
    // Ordinary words stay intact.
    assert_eq!(
        s("a file-based store uses file_search"),
        "a file-based store uses file_search"
    );
    assert_eq!(s("vs_short"), "vs_short");
}

#[test]
fn replaces_urls_and_credentials() {
    assert_eq!(
        s("see https://api.openai.com/v1/files?x=1 now"),
        "see [url] now"
    );
    assert_eq!(s("key sk-abcdefghij123 leaked"), "key [credential] leaked");
    assert_eq!(s("sk-short"), "sk-short");
    assert_eq!(
        s("Authorization: Bearer eyJhbGc.x.y"),
        "Authorization: [credential]"
    );
}

#[test]
fn keeps_plain_text() {
    assert_eq!(s("The model is overloaded"), "The model is overloaded");
}
