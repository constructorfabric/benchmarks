//! Provider message sanitization (DESIGN 3.3 "Provider identifier non-exposure
//! invariant"): every recognized provider identifier, URL and credential is
//! replaced before a provider message reaches a client; everything else is
//! left as is.
//!
//! Public API under test (see `S3-test-interface.md` section 2.1):
//! `mini_chat::infra::llm::sanitize::sanitize_provider_message(&str) -> String`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat::infra::llm::sanitize::sanitize_provider_message as sanitize;

#[test]
fn response_and_completion_ids_become_provider_id() {
    for id in [
        "resp_abc123DEF456ghi789",
        "chatcmpl-9Xk2Lm4Nq7Rs",
        "cmpl-AbCdEf123456",
        "msg_0123456789abcdef",
    ] {
        let out = sanitize(&format!("request {id} failed"));
        assert_eq!(out, "request [provider_id] failed", "{id}");
    }
}

#[test]
fn file_and_vector_store_ids_become_provider_id() {
    for id in [
        "file-abcdef123456",
        "file_abcdef123456",
        "assistant-abcdef123456",
        "vs_abcdef123456",
        "vs_0123456789ABCDEFxyz",
    ] {
        let out = sanitize(&format!("object {id} not found"));
        assert_eq!(out, "object [provider_id] not found", "{id}");
    }
}

#[test]
fn file_like_ids_need_at_least_twelve_alphanumerics_after_the_prefix() {
    // The length floor keeps ordinary words intact.
    for text in [
        "a file-based approach",
        "the file_search tool is unavailable",
        "an assistant-like tone",
        "vs_short",
        "file-12345678901",
    ] {
        assert_eq!(sanitize(text), text, "`{text}` is not a provider id");
    }
    assert_eq!(sanitize("file-123456789012"), "[provider_id]");
}

#[test]
fn urls_become_url() {
    let out = sanitize("see https://platform.openai.com/docs/errors?x=1 for details");
    assert!(out.contains("[url]"), "{out}");
    assert!(!out.contains("platform.openai.com"), "{out}");
    assert!(out.starts_with("see "), "{out}");
    assert!(out.ends_with(" for details"), "{out}");

    let out = sanitize("GET http://10.0.0.5:8080/internal failed");
    assert!(out.contains("[url]") && !out.contains("10.0.0.5"), "{out}");
}

#[test]
fn api_keys_and_bearer_tokens_become_credential() {
    let out = sanitize("Incorrect API key provided: sk-abcdefghij1234567890.");
    assert!(out.contains("[credential]"), "{out}");
    assert!(!out.contains("sk-abcdefghij"), "{out}");

    let out = sanitize("invalid header Authorization: Bearer eyJhbGciOi.payload-part_1.sig");
    assert!(out.contains("[credential]"), "{out}");
    assert!(!out.contains("eyJhbGciOi"), "{out}");
    assert!(!out.contains("Bearer eyJ"), "{out}");
}

#[test]
fn an_sk_key_is_recognized_from_ten_alphanumerics() {
    assert_eq!(sanitize("sk-abcdefghij"), "[credential]");
    assert_eq!(
        sanitize("sk-abcdefghi"),
        "sk-abcdefghi",
        "nine is not a key"
    );
}

#[test]
fn every_occurrence_is_replaced_and_the_surrounding_text_is_kept() {
    let input = "first resp_aaaaaaaaaaaa then file-bbbbbbbbbbbb and vs_cccccccccccc, again resp_aaaaaaaaaaaa";
    let out = sanitize(input);
    assert_eq!(
        out,
        "first [provider_id] then [provider_id] and [provider_id], again [provider_id]"
    );
}

#[test]
fn mixed_message_replaces_each_kind_with_its_marker() {
    let out = sanitize(
        "Error for resp_abcdefabcdef using sk-abcdefghij12 at https://api.openai.com/v1/responses; file-abcdefabcdef missing",
    );
    assert!(out.contains("[provider_id]"), "{out}");
    assert!(out.contains("[credential]"), "{out}");
    assert!(out.contains("[url]"), "{out}");
    for raw in [
        "resp_abcdefabcdef",
        "sk-abcdefghij12",
        "api.openai.com",
        "file-abcdefabcdef",
    ] {
        assert!(!out.contains(raw), "`{raw}` leaked: {out}");
    }
    assert!(out.starts_with("Error for "), "{out}");
    assert!(out.ends_with(" missing"), "{out}");
}

#[test]
fn plain_text_is_unchanged() {
    for text in [
        "",
        "The server had an error while processing your request.",
        "rate limit reached for requests",
        "context_length_exceeded: maximum context length is 8192 tokens",
        "Unicode stays: caf\u{e9} \u{4e2d}\u{6587} \u{1f600}",
    ] {
        assert_eq!(sanitize(text), text);
    }
}

#[test]
fn sanitizing_twice_changes_nothing() {
    let once = sanitize("resp_abcdefabcdef at https://x.example/y with sk-abcdefghij12");
    assert_eq!(sanitize(&once), once);
}
