use super::*;

#[test]
fn scrubs_provider_ids() {
    let m = sanitize_provider_message(
        "Response resp_abc123 failed for file-AbCdEf1234567890 in vs_0123456789abcdef via assistant-ABCDEFGHIJKLMN",
    );
    assert!(!m.contains("resp_abc123"));
    assert!(!m.contains("file-AbCdEf"));
    assert!(!m.contains("vs_0123"));
    assert!(!m.contains("assistant-ABC"));
    assert_eq!(m.matches("[provider_id]").count(), 4);
}

#[test]
fn keeps_ordinary_words() {
    let m = sanitize_provider_message("file-based retrieval with file_search failed");
    assert_eq!(m, "file-based retrieval with file_search failed");
}

#[test]
fn scrubs_urls_and_credentials() {
    let m = sanitize_provider_message(
        "see https://api.openai.com/v1/files?x=1 key sk-abcdefghijklmnop Authorization: Bearer abc.def",
    );
    assert!(m.contains("[url]"));
    assert!(!m.contains("api.openai.com"));
    assert!(!m.contains("sk-abcdef"));
    assert!(!m.contains("abc.def"));
    assert!(m.contains("[credential]"));
}

#[test]
fn scrubs_completion_ids() {
    assert_eq!(
        sanitize_provider_message("chatcmpl-XYZ1 cmpl-9 msg_01ab"),
        "[provider_id] [provider_id] [provider_id]"
    );
}
