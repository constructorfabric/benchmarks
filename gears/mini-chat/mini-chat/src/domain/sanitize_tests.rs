use super::sanitize_provider_message as s;

#[test]
fn response_and_completion_ids_are_replaced() {
    assert_eq!(
        s("bad thing resp_abc123 happened"),
        "bad thing [provider_id] happened"
    );
    assert_eq!(s("id chatcmpl-AbC9 failed"), "id [provider_id] failed");
    assert_eq!(s("cmpl-x1"), "[provider_id]");
    assert_eq!(s("(msg_01XyZ)"), "([provider_id])");
}

#[test]
fn file_and_vector_store_ids_need_twelve_alnum() {
    assert_eq!(
        s("file file-abcdefghijkl12 missing"),
        "file [provider_id] missing"
    );
    assert_eq!(s("file_ABCDEFGHIJKL"), "[provider_id]");
    assert_eq!(s("assistant-0123456789ab"), "[provider_id]");
    assert_eq!(
        s("store vs_abc123def456ghi not found"),
        "store [provider_id] not found"
    );
    // Ordinary words and short ids stay untouched.
    assert_eq!(s("file-based storage"), "file-based storage");
    assert_eq!(s("the file_search tool"), "the file_search tool");
    assert_eq!(s("vs_short and file-abc123"), "vs_short and file-abc123");
    assert_eq!(s("assistant-mode"), "assistant-mode");
}

#[test]
fn urls_are_replaced() {
    assert_eq!(
        s("bad thing resp_abc123 at https://x.y/z"),
        "bad thing [provider_id] at [url]"
    );
    assert_eq!(s("see http://example.com/a?b=c, then"), "see [url], then");
    assert_eq!(
        s("(https://api.openai.com/v1/files/file-abcdefghijklmnop)"),
        "([url])"
    );
}

#[test]
fn credentials_are_replaced() {
    assert_eq!(
        s("Incorrect API key provided: sk-abcdefghij1234"),
        "Incorrect API key provided: [credential]"
    );
    assert_eq!(
        s("key sk-proj-abcdefghijklmnop_xyz used"),
        "key [credential] used"
    );
    assert_eq!(s("sk-short"), "sk-short");
    assert_eq!(s("task-abcdefghijklmnop"), "task-abcdefghijklmnop");
    assert_eq!(
        s("header Authorization: Bearer abc.def-ghi_123"),
        "header Authorization: Bearer [credential]"
    );
    assert_eq!(s("bearer tok123"), "bearer [credential]");
}

#[test]
fn other_text_is_unchanged() {
    let msg = "The server had an error while processing your request. Sorry about that!";
    assert_eq!(s(msg), msg);
    assert_eq!(s(""), "");
    assert_eq!(s("respond_to msg rate"), "respond_to msg rate");
}

#[test]
fn combined_message() {
    let out = s("resp_1 / file-AAAAAAAAAAAA1 / https://h/p / sk-1234567890 / Bearer xyz");
    assert_eq!(
        out,
        "[provider_id] / [provider_id] / [url] / [credential] / Bearer [credential]"
    );
}
