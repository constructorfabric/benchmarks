use super::*;

#[test]
fn scrubs_provider_ids() {
    let s = sanitize_provider_message(
        "failed for file-abcdef0123456789 in vs_ABCDEFGHIJKL12 (resp_123abc, chatcmpl-9x, msg_01A, assistant-0123456789ab)",
    );
    assert_eq!(
        s,
        "failed for [provider_id] in [provider_id] ([provider_id], [provider_id], [provider_id], [provider_id])"
    );
}

#[test]
fn keeps_ordinary_words() {
    let s = sanitize_provider_message("file-based storage and file_search tool failed");
    assert_eq!(s, "file-based storage and file_search tool failed");
}

#[test]
fn scrubs_urls_and_credentials() {
    let s = sanitize_provider_message(
        "see https://api.openai.com/v1/files?x=1 key sk-abcdefghijklmnop and Bearer eyJabc.def",
    );
    assert_eq!(s, "see [url] key [credential] and [credential]");
    assert_eq!(sanitize_provider_message("sk-short"), "sk-short");
}
