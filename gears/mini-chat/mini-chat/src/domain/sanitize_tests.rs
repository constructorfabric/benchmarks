use super::*;

#[test]
fn replaces_provider_ids_urls_and_credentials() {
    let cases = vec![
        (
            "File file-AbCdEf1234567890 not found",
            "File [provider_id] not found",
        ),
        ("vector store vs_abc123def456ghi missing", "vector store [provider_id] missing"),
        ("assistant-ABCDEFGHIJKLMN failed", "[provider_id] failed"),
        ("anthropic file_0123456789abcdef", "anthropic [provider_id]"),
        ("response resp_abc123 failed", "response [provider_id] failed"),
        ("chatcmpl-xyz9 error", "[provider_id] error"),
        ("see https://api.openai.com/v1/files?x=1 now", "see [url] now"),
        ("key sk-proj1234567890abcdef leaked", "key [credential] leaked"),
        ("Authorization: Bearer abc.def.ghi", "Authorization: [credential]"),
    ];
    for (input, expected) in cases {
        assert_eq!(sanitize_provider_message(input), expected, "input: {input}");
    }
}

#[test]
fn keeps_ordinary_words() {
    let msg = "file-based storage and file_search results; vs_short; sk-short";
    assert_eq!(sanitize_provider_message(msg), msg);
}
