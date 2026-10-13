use super::{
    CREDENTIAL_PLACEHOLDER, PROVIDER_ID_PLACEHOLDER, URL_PLACEHOLDER, sanitize_provider_message,
};

fn sanitize(message: &str) -> String {
    sanitize_provider_message(message)
}

#[test]
fn full_provider_message_is_sanitized_exactly() {
    let raw = "Upstream failure for response resp_abc123def456ghi789 (msg_ZyXw9876543210) \
               while reading file-AbCdEf123456789012 from vs_QwErTy1234567890 \
               with key sk-AbCdEfGhIjKlMnOp0123 at https://internal-llm.example.com/v1/responses \
               and file-based file_search was skipped";
    let expected = "Upstream failure for response [provider_id] ([provider_id]) \
                    while reading [provider_id] from [provider_id] \
                    with key [credential] at [url] \
                    and file-based file_search was skipped";
    assert_eq!(sanitize(raw), expected);
}

#[test]
fn default_mock_message_is_sanitized() {
    let raw = "Upstream failure for response resp_abc123def456ghi789 while reading \
               file-AbCdEf123456789012 with key sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123 \
               at https://internal-llm.example.com/v1/responses";
    assert_eq!(
        sanitize(raw),
        "Upstream failure for response [provider_id] while reading \
         [provider_id] with key [credential] at [url]"
    );
}

#[test]
fn response_and_completion_ids_are_replaced() {
    for id in [
        "resp_68af2c1d",
        "chatcmpl-B9MBs8CjcvOU2jLn4n570S5qMJKcT",
        "cmpl-7QmVI15qgYVllxK0FtxVGG6ywfzaq",
        "msg_01XFDUDYJgAACzvnptvVoYEL",
        "msg_a",
    ] {
        assert_eq!(
            sanitize(&format!("id {id} failed")),
            "id [provider_id] failed",
            "{id}"
        );
    }
}

#[test]
fn file_and_vector_store_ids_need_twelve_characters() {
    for id in [
        "file-AbCdEf123456",
        "file_AbCdEf123456",
        "assistant-AbCdEf123456",
        "vs_AbCdEf123456",
    ] {
        assert_eq!(sanitize(id), PROVIDER_ID_PLACEHOLDER, "{id}");
    }
    for word in [
        "file-AbCdEf12345",
        "vs_short",
        "file-based",
        "file_search",
        "assistant-mode",
    ] {
        assert_eq!(sanitize(word), word, "{word}");
    }
}

#[test]
fn ids_inside_words_are_left_alone() {
    for text in ["myresp_abc123", "xmsg_abc", "prefile-AbCdEf1234567890"] {
        assert_eq!(sanitize(text), text, "{text}");
    }
}

#[test]
fn secret_keys_need_ten_letters_or_digits() {
    assert_eq!(sanitize("sk-AbCdEfGhIj"), CREDENTIAL_PLACEHOLDER);
    assert_eq!(sanitize("sk-proj-Ab_Cd-EfGhIjKl"), CREDENTIAL_PLACEHOLDER);
    assert_eq!(sanitize("sk-AbCdEfGhI"), "sk-AbCdEfGhI");
    assert_eq!(
        sanitize("risk-free task-123456789012"),
        "risk-free task-123456789012"
    );
}

#[test]
fn bearer_tokens_are_replaced() {
    assert_eq!(
        sanitize("header Authorization: Bearer eyJhbGciOi.payload.sig rejected"),
        "header Authorization: [credential] rejected"
    );
    assert_eq!(sanitize("bearer abc"), CREDENTIAL_PLACEHOLDER);
}

#[test]
fn urls_are_replaced_without_trailing_punctuation() {
    assert_eq!(
        sanitize("see https://platform.openai.com/docs/errors."),
        format!("see {URL_PLACEHOLDER}.")
    );
    assert_eq!(
        sanitize("(http://10.0.0.1:8080/v1/files?x=1), retry"),
        "([url]), retry"
    );
    assert_eq!(
        sanitize("url https://h.example/resp_abc123/file-AbCdEf1234567890 end"),
        "url [url] end"
    );
}

#[test]
fn ordinary_text_is_unchanged() {
    let text = "The model is overloaded. Please retry in 30 seconds; code=server_error.";
    assert_eq!(sanitize(text), text);
    assert_eq!(sanitize(""), "");
}

#[test]
fn sanitization_is_idempotent() {
    let once = sanitize("resp_abc https://x.example sk-AbCdEfGhIjKl file-AbCdEf123456");
    assert_eq!(sanitize(&once), once);
    assert_eq!(once, "[provider_id] [url] [credential] [provider_id]");
}
