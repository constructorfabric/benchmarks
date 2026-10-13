#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::non_ascii_literal)]
//! Slice 4: error message sanitization
//! (`cpt-cf-mini-chat-algo-streaming-turns-sanitize-error`), the DESIGN
//! patterns, the widening of the feature and the masked-key, `api-key` and
//! `Authorization` extensions.

use super::*;

fn check(raw: &str, expected: &str) {
    assert_eq!(sanitize_error(raw), expected, "input: {raw}");
}

fn assert_gone(raw: &str, secret: &str) {
    let clean = sanitize_error(raw);
    assert!(!clean.contains(secret), "`{secret}` survived in `{clean}`");
}

// ---- urls ---------------------------------------------------------------------------------------

#[test]
fn urls_are_replaced_up_to_the_next_whitespace() {
    check("see https://x.example/a?sig=abc#z now", "see [url] now");
    check("http://x.example failed", "[url] failed");
}

#[test]
fn urls_with_user_info_or_a_query_string_leave_nothing_behind() {
    let clean = sanitize_error("fetch https://user:hunter2@host.example/p?token=abc123&x=1 failed");
    assert_eq!(clean, "fetch [url] failed");
}

// ---- bearer tokens and keys ---------------------------------------------------------------------

#[test]
fn bearer_tokens_are_replaced_in_any_letter_case() {
    check(
        "Authorization failed: Bearer abc.def-ghi",
        "Authorization failed: [credential]",
    );
    check("got bearer xyz123 here", "got [credential] here");
    check("got BEARER TOKEN123 here", "got [credential] here");
    check("got BeArEr\ttok.en here", "got [credential] here");
}

#[test]
fn secret_keys_are_replaced() {
    check("key sk-ABCDEFGHIJ12 is wrong", "key [credential] is wrong");
    check(
        "key sk-proj-AbC_12-xyZ987 is wrong",
        "key [credential] is wrong",
    );
}

#[test]
fn masked_keys_are_replaced() {
    check(
        "Incorrect API key provided: sk-proj-********************abcd, see docs",
        "Incorrect API key provided: [credential], see docs",
    );
    check("bad key sk-****", "bad key [credential]");
    check("bad key sk-ab.*_-cd done", "bad key [credential] done");
}

#[test]
fn a_secret_key_prefix_inside_a_word_is_not_a_key() {
    check("a task-based flow", "a task-based flow");
    check("the desk-lamp", "the desk-lamp");
    check("sk-ab", "sk-ab");
}

#[test]
fn api_key_header_values_are_replaced_and_the_name_stays() {
    check("api-key: abc123def failed", "api-key: [credential] failed");
    check("API-KEY=abc123def, retry", "API-KEY=[credential], retry");
    check("Api-Key : abc123def", "Api-Key : [credential]");
}

#[test]
fn authorization_header_values_are_replaced_and_the_name_stays() {
    check(
        "Authorization: Basic dXNlcjpwYXNzd29yZA== was refused",
        "Authorization: [credential] was refused",
    );
    check(
        "authorization: abc123 failed",
        "authorization: [credential] failed",
    );
    check(
        "AUTHORIZATION=Token tk_12345 failed",
        "AUTHORIZATION=[credential] failed",
    );
    check(
        "Authorization: Bearer abc.def refused",
        "Authorization: [credential] refused",
    );
}

// ---- x-request-id and uuids ---------------------------------------------------------------------

#[test]
fn the_x_request_id_value_is_replaced_and_the_name_stays() {
    check(
        "x-request-id: abc123 more",
        "x-request-id: [provider_id] more",
    );
    check("X-REQUEST-ID=abc123;", "X-REQUEST-ID=[provider_id];");
    check(
        "X-Request-Id: \"abc123\", next",
        "X-Request-Id: \"[provider_id]\", next",
    );
}

#[test]
fn uuids_are_replaced_in_any_letter_case() {
    check(
        "request 3f2b8c1e-9a4d-4e7f-b6a1-0c5d2e8f9a7b failed",
        "request [provider_id] failed",
    );
    check(
        "request 3F2B8C1E-9A4D-4E7F-B6A1-0C5D2E8F9A7B failed",
        "request [provider_id] failed",
    );
}

// ---- provider ids -------------------------------------------------------------------------------

#[test]
fn response_and_completion_ids_are_replaced() {
    check("see resp_9xY2 now", "see [provider_id] now");
    check("see chatcmpl-ABC123def now", "see [provider_id] now");
    check("see cmpl-abc123 now", "see [provider_id] now");
    check("see msg_01XyZ now", "see [provider_id] now");
}

#[test]
fn prefixed_ids_with_a_digit_are_replaced() {
    for id in [
        "req_7f3a9b2c1d",
        "org-abcDEF123",
        "proj_x1y2z3w",
        "call_abc123def",
        "fc_abc123def",
        "rs_abc123def",
        "ws_abc123def",
        "ci_abc123def",
        "toolu_01ABCdef",
        "thread_abc123",
        "run_abc123",
        "step_abc123",
    ] {
        check(
            &format!("failed for {id} today"),
            "failed for [provider_id] today",
        );
    }
}

#[test]
fn ordinary_words_with_the_same_prefixes_stay_intact() {
    for text in [
        "file-based storage",
        "uses file_search here",
        "a step-by-step guide",
        "call_back_later please",
        "run_time_error happened",
        "thread_safety matters",
        "org-chart",
    ] {
        check(text, text);
    }
}

#[test]
fn file_and_vector_store_ids_need_twelve_characters() {
    check(
        "Invalid file file-abc123def456ghi here",
        "Invalid file [provider_id] here",
    );
    check("store vs_abcdef123456 gone", "store [provider_id] gone");
    check(
        "asst assistant-abc123def456 gone",
        "asst [provider_id] gone",
    );
    check("file_abcdef123456 gone", "[provider_id] gone");
    // Eleven characters is not an id of this kind.
    check("file-abcdefghijk", "file-abcdefghijk");
}

// ---- the whole message --------------------------------------------------------------------------

#[test]
fn the_acceptance_message_is_cleaned_completely() {
    let raw = "Invalid file file-abc123def456ghi in resp_9xY2 see https://x.example/a with key sk-ABCDEFGHIJ12";
    let clean = sanitize_error(raw);
    assert_eq!(
        clean,
        "Invalid file [provider_id] in [provider_id] see [url] with key [credential]"
    );
    for raw_value in [
        "file-abc123def456ghi",
        "resp_9xY2",
        "https://x.example/a",
        "sk-ABCDEFGHIJ12",
    ] {
        assert_gone(raw, raw_value);
    }
}

#[test]
fn a_message_without_identifiers_is_unchanged() {
    check(
        "The model `gpt-5` is overloaded, please try again later.",
        "The model `gpt-5` is overloaded, please try again later.",
    );
    check("", "");
}

#[test]
fn sanitizing_twice_changes_nothing() {
    let raw = "Bearer abc resp_9xY2 https://x.example/a sk-ABCDEFGHIJ12 req_7f3a9b2c1d";
    let once = sanitize_error(raw);
    assert_eq!(sanitize_error(&once), once);
}

// ---- error_detail -------------------------------------------------------------------------------

#[test]
fn the_error_detail_is_the_provider_code_and_the_sanitized_message() {
    assert_eq!(
        error_detail(Some("invalid_request_error"), "bad id resp_9xY2"),
        "invalid_request_error: bad id [provider_id]"
    );
}

#[test]
fn the_error_detail_without_a_code_is_the_sanitized_message() {
    assert_eq!(
        error_detail(None, "bad id resp_9xY2"),
        "bad id [provider_id]"
    );
}

#[test]
fn the_error_detail_is_cut_after_sanitizing_and_never_leaves_an_id_fragment() {
    // An id that spans character 1024 of the raw text.
    let uuid = "3f2b8c1e-9a4d-4e7f-b6a1-0c5d2e8f9a7b";
    let raw = format!("{} {uuid} tail", "x".repeat(1010));
    let detail = error_detail(Some("provider_error"), &raw);
    assert!(
        detail.chars().count() <= ERROR_DETAIL_MAX_CHARS,
        "{}",
        detail.len()
    );
    assert!(detail.starts_with("provider_error: "));
    for fragment in ["3f2b8c1e", "9a4d", "4e7f", "b6a1", "0c5d2e8f9a7b"] {
        assert!(!detail.contains(fragment), "`{fragment}` survived");
    }
}

#[test]
fn a_short_error_detail_is_not_cut() {
    let detail = error_detail(Some("c"), &"y".repeat(500));
    assert_eq!(detail.chars().count(), 3 + 500);
}

#[test]
fn the_cut_counts_characters_not_bytes() {
    let detail = error_detail(None, &"é".repeat(2000));
    assert_eq!(detail.chars().count(), ERROR_DETAIL_MAX_CHARS);
}
