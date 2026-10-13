use super::sanitize_provider_message as s;

#[test]
fn replaces_provider_ids_urls_and_credentials() {
    assert_eq!(
        s("bad file file-abcdefghijklmnop at https://x.y/z"),
        "bad file [provider_id] at [url]"
    );
    assert_eq!(s("response resp_abc123 failed"), "response [provider_id] failed");
    assert_eq!(s("vector store vs_ABCDEFGHIJKL1 missing"), "vector store [provider_id] missing");
    assert_eq!(s("key sk-abcdefghij123 invalid"), "key [credential] invalid");
    assert_eq!(s("Authorization: Bearer abc.def-ghi"), "Authorization: [credential]");
    assert_eq!(s("chatcmpl-XYZ and msg_01abc"), "[provider_id] and [provider_id]");
}

#[test]
fn keeps_ordinary_words() {
    assert_eq!(s("file-based storage and file_search tool"), "file-based storage and file_search tool");
    assert_eq!(s("Provider is currently unavailable"), "Provider is currently unavailable");
}
