//! The reading of the non-streaming summary answer with recorded fixtures per adapter kind: a
//! Responses body with a `reasoning` item and an `incomplete` status, a vLLM body with a `<think>`
//! block, a Chat Completions body with null content and with `finish_reason` `length`, an
//! Anthropic body with a `thinking` block and with `stop_reason` `max_tokens`, the usage per kind
//! (cached, reasoning and cache-write counts, absent counts, an absent usage object), the failure
//! mapping (malformed body, a Responses `failed` status or `error`, an empty text as a success)
//! and the classification of a non-2xx answer (the context-length wordings of `OpenAI`, Anthropic
//! and vLLM, a 400 with an unrelated message, a 429, a sanitized cause).
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call` (`inst-ad-non-2xx`,
//! `inst-ad-read-error`, `inst-ad-classify-ptl`, `inst-ad-return-ptl`, `inst-ad-return-other`,
//! `inst-ad-parse`, `inst-ad-malformed`, `inst-ad-return-malformed`, `inst-ad-extract-match`,
//! `inst-ad-text-responses`, `inst-ad-text-chat-completions`, `inst-ad-text-anthropic`,
//! `inst-ad-usage-match`, `inst-ad-usage-responses`, `inst-ad-usage-chat-completions`,
//! `inst-ad-usage-anthropic`, `inst-ad-empty-text`, `inst-ad-no-content-logs`, `inst-ad-return`);
//! definition of done `cpt-cf-mini-chat-dod-background-maintenance-summary-execution`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::UsageTokens;
use serde_json::{Value, json};

use super::{SummaryCallFailure, SummaryCompletion, classify_error_response, parse_completion};
use crate::config::ProviderKind;

const MALFORMED: &str = "the provider response is malformed";

fn parse(kind: ProviderKind, body: &Value) -> Result<SummaryCompletion, SummaryCallFailure> {
    parse_completion(kind, body.to_string().as_bytes())
}

fn ok(kind: ProviderKind, body: &Value) -> SummaryCompletion {
    parse(kind, body).unwrap_or_else(|failure| panic!("{kind:?}: {failure:?}"))
}

fn usage(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: reasoning,
    }
}

fn is_malformed(result: &Result<SummaryCompletion, SummaryCallFailure>) -> bool {
    matches!(result, Err(SummaryCallFailure::Other { cause }) if cause == MALFORMED)
}

fn is_other(result: &Result<SummaryCompletion, SummaryCallFailure>) -> bool {
    matches!(result, Err(SummaryCallFailure::Other { .. }))
}

fn message_item(parts: &[&str]) -> Value {
    json!({ "type": "message", "role": "assistant",
             "content": parts.iter().map(|text| json!({ "type": "output_text", "text": text }))
                 .collect::<Vec<_>>() })
}

const RESPONSES_KINDS: [ProviderKind; 2] =
    [ProviderKind::OpenaiResponses, ProviderKind::VllmResponses];
const ALL_KINDS: [ProviderKind; 4] = [
    ProviderKind::OpenaiResponses,
    ProviderKind::OpenaiChatCompletions,
    ProviderKind::VllmResponses,
    ProviderKind::AnthropicMessages,
];

// ── Responses and vLLM ───────────────────────────────────────────────────────

#[test]
fn the_text_parts_of_the_message_items_are_joined_in_order() {
    // inst-ad-extract-match, inst-ad-text-responses
    for kind in RESPONSES_KINDS {
        let body = json!({ "status": "completed",
            "output": [message_item(&["Hel", "lo"]), message_item(&[", world"])] });
        assert_eq!(ok(kind, &body).text, "Hello, world", "{kind:?}");
    }
}

#[test]
fn a_reasoning_item_and_every_other_item_are_ignored() {
    // inst-ad-text-responses: a `reasoning` item, a tool call and a refusal part
    for kind in RESPONSES_KINDS {
        let body = json!({ "status": "completed", "output": [
            { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "HIDDEN THOUGHT" }] },
            { "type": "web_search_call", "id": "ws_1", "status": "completed" },
            { "type": "message", "role": "assistant", "content": [
                { "type": "refusal", "refusal": "REFUSAL" },
                { "type": "output_text", "text": "the answer" } ] },
            { "type": "function_call", "name": "x", "arguments": "{}" }
        ]});
        assert_eq!(ok(kind, &body).text, "the answer", "{kind:?}");
    }
}

#[test]
fn an_incomplete_status_is_a_truncated_but_valid_completion() {
    // inst-ad-text-responses: its text is used as it is
    for kind in RESPONSES_KINDS {
        let body = json!({ "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": [message_item(&["cut off here"])],
            "usage": { "input_tokens": 10, "output_tokens": 99 } });
        let completion = ok(kind, &body);
        assert_eq!(completion.text, "cut off here", "{kind:?}");
        assert_eq!(completion.usage.output_tokens, 99);
    }
}

#[test]
fn an_output_that_is_not_an_array_is_malformed() {
    // inst-ad-text-responses, inst-ad-malformed
    for kind in RESPONSES_KINDS {
        for body in [
            json!({ "status": "completed" }),
            json!({ "status": "completed", "output": "text" }),
            json!({ "status": "completed", "output": { "type": "message" } }),
            json!({ "status": "completed", "output": null }),
        ] {
            assert!(is_malformed(&parse(kind, &body)), "{kind:?} {body}");
        }
    }
}

#[test]
fn a_failed_status_is_a_failure_of_the_class_other() {
    // inst-ad-text-responses
    for kind in RESPONSES_KINDS {
        let body = json!({ "status": "failed", "output": [message_item(&["text"])],
            "error": { "code": "server_error", "message": "PROVIDER-SECRET-TEXT" } });
        let result = parse(kind, &body);
        assert!(is_other(&result), "{kind:?}");
        let Err(SummaryCallFailure::Other { cause }) = result else {
            unreachable!()
        };
        assert!(!cause.contains("PROVIDER-SECRET-TEXT"), "{cause}");
        let only_status = json!({ "status": "failed", "output": [] });
        assert!(is_other(&parse(kind, &only_status)), "{kind:?}");
    }
}

#[test]
fn a_non_null_error_is_a_failure_and_a_null_error_is_not() {
    // inst-ad-text-responses
    for kind in RESPONSES_KINDS {
        let failed = json!({ "status": "completed", "output": [message_item(&["x"])],
                              "error": { "code": "boom" } });
        assert!(is_other(&parse(kind, &failed)), "{kind:?}");
        let fine = json!({ "status": "completed", "output": [message_item(&["x"])],
                            "error": null });
        assert_eq!(ok(kind, &fine).text, "x", "{kind:?}");
    }
}

#[test]
fn a_vllm_think_block_is_removed_from_the_joined_text() {
    // inst-ad-text-responses: the streaming adapter treats it as reasoning
    let body = json!({ "status": "completed", "output": [message_item(&[
        "<think>step one\nstep two</think>The answer.<think>again</think> Done."
    ])]});
    assert_eq!(
        ok(ProviderKind::VllmResponses, &body).text,
        "The answer. Done."
    );
    let split = json!({ "status": "completed", "output": [message_item(&[
        "<think>part a", " and b</think>visible"
    ])]});
    assert_eq!(ok(ProviderKind::VllmResponses, &split).text, "visible");
}

#[test]
fn the_openai_responses_adapter_keeps_a_think_tag_as_text() {
    // inst-ad-text-responses: the removal is for `vllm_responses` only
    let body =
        json!({ "status": "completed", "output": [message_item(&["<think>x</think>kept"])] });
    assert_eq!(
        ok(ProviderKind::OpenaiResponses, &body).text,
        "<think>x</think>kept"
    );
}

#[test]
fn the_responses_usage_names_the_cached_and_reasoning_details() {
    // inst-ad-usage-responses: cache_write is 0
    for kind in RESPONSES_KINDS {
        let body = json!({ "status": "completed", "output": [message_item(&["t"])],
            "usage": { "input_tokens": 1200, "output_tokens": 345,
                        "input_tokens_details": { "cached_tokens": 800 },
                        "output_tokens_details": { "reasoning_tokens": 100 } } });
        assert_eq!(
            ok(kind, &body).usage,
            usage(1200, 345, 800, 0, 100),
            "{kind:?}"
        );
    }
}

#[test]
fn absent_responses_counts_are_zero_and_an_absent_usage_object_is_all_zero() {
    // inst-ad-usage-match
    for kind in RESPONSES_KINDS {
        let partial = json!({ "status": "completed", "output": [message_item(&["t"])],
            "usage": { "input_tokens": 7 } });
        assert_eq!(ok(kind, &partial).usage, usage(7, 0, 0, 0, 0), "{kind:?}");
        let none = json!({ "status": "completed", "output": [message_item(&["t"])] });
        assert_eq!(ok(kind, &none).usage, UsageTokens::default(), "{kind:?}");
        let null =
            json!({ "status": "completed", "output": [message_item(&["t"])], "usage": null });
        assert_eq!(ok(kind, &null).usage, UsageTokens::default(), "{kind:?}");
    }
}

// ── Chat Completions ─────────────────────────────────────────────────────────

fn chat_body(content: &Value, finish: &str) -> Value {
    json!({ "choices": [{ "index": 0, "message": { "role": "assistant", "content": content },
                           "finish_reason": finish }] })
}

#[test]
fn the_chat_completions_text_is_the_first_choice_content() {
    // inst-ad-text-chat-completions
    let mut body = chat_body(&json!("the summary"), "stop");
    body["choices"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "index": 1, "message": { "role": "assistant", "content": "ignored" } }));
    assert_eq!(
        ok(ProviderKind::OpenaiChatCompletions, &body).text,
        "the summary"
    );
}

#[test]
fn null_or_absent_content_is_an_empty_text() {
    // inst-ad-text-chat-completions, inst-ad-empty-text: a success with an empty text
    let kind = ProviderKind::OpenaiChatCompletions;
    assert_eq!(ok(kind, &chat_body(&Value::Null, "stop")).text, "");
    let absent = json!({ "choices": [{ "index": 0, "message": { "role": "assistant" } }] });
    assert_eq!(ok(kind, &absent).text, "");
}

#[test]
fn a_length_finish_reason_is_a_truncated_but_valid_completion() {
    // inst-ad-text-chat-completions
    let body = chat_body(&json!("<summary>cut"), "length");
    assert_eq!(
        ok(ProviderKind::OpenaiChatCompletions, &body).text,
        "<summary>cut"
    );
}

#[test]
fn choices_that_are_not_a_non_empty_array_are_malformed() {
    // inst-ad-text-chat-completions, inst-ad-malformed
    let kind = ProviderKind::OpenaiChatCompletions;
    for body in [
        json!({}),
        json!({ "choices": [] }),
        json!({ "choices": {} }),
        json!({ "choices": "x" }),
        json!({ "choices": null }),
    ] {
        assert!(is_malformed(&parse(kind, &body)), "{body}");
    }
}

#[test]
fn the_chat_completions_usage_names_prompt_and_completion_tokens() {
    // inst-ad-usage-chat-completions: cache_write is 0
    let mut body = chat_body(&json!("t"), "stop");
    body["usage"] = json!({
        "prompt_tokens": 500, "completion_tokens": 60,
        "prompt_tokens_details": { "cached_tokens": 128 },
        "completion_tokens_details": { "reasoning_tokens": 20 }
    });
    assert_eq!(
        ok(ProviderKind::OpenaiChatCompletions, &body).usage,
        usage(500, 60, 128, 0, 20)
    );
}

#[test]
fn absent_chat_counts_are_zero_and_an_absent_usage_object_is_all_zero() {
    // inst-ad-usage-match
    let kind = ProviderKind::OpenaiChatCompletions;
    let mut partial = chat_body(&json!("t"), "stop");
    partial["usage"] = json!({ "completion_tokens": 9 });
    assert_eq!(ok(kind, &partial).usage, usage(0, 9, 0, 0, 0));
    assert_eq!(
        ok(kind, &chat_body(&json!("t"), "stop")).usage,
        UsageTokens::default()
    );
}

// ── Anthropic ────────────────────────────────────────────────────────────────

fn anthropic_body(blocks: &Value, stop: &str) -> Value {
    json!({ "type": "message", "role": "assistant", "content": blocks, "stop_reason": stop })
}

#[test]
fn the_anthropic_text_blocks_are_joined_and_other_blocks_ignored() {
    // inst-ad-text-anthropic: `thinking`, `tool_use` and every other block are ignored
    let body = anthropic_body(
        &json!([
            { "type": "thinking", "thinking": "HIDDEN", "signature": "s" },
            { "type": "text", "text": "Hello" },
            { "type": "tool_use", "id": "t1", "name": "x", "input": {} },
            { "type": "text", "text": ", world" },
            { "type": "redacted_thinking", "data": "xx" }
        ]),
        "end_turn",
    );
    assert_eq!(
        ok(ProviderKind::AnthropicMessages, &body).text,
        "Hello, world"
    );
}

#[test]
fn an_anthropic_max_tokens_stop_is_a_truncated_but_valid_completion() {
    // inst-ad-text-anthropic
    let body = anthropic_body(&json!([{ "type": "text", "text": "cut" }]), "max_tokens");
    assert_eq!(ok(ProviderKind::AnthropicMessages, &body).text, "cut");
}

#[test]
fn an_error_type_or_content_that_is_not_an_array_is_malformed() {
    // inst-ad-text-anthropic, inst-ad-malformed
    let kind = ProviderKind::AnthropicMessages;
    for body in [
        json!({ "type": "error", "error": { "type": "overloaded_error", "message": "x" } }),
        json!({ "type": "message", "content": "text" }),
        json!({ "type": "message" }),
        json!({ "type": "message", "content": null }),
    ] {
        assert!(is_malformed(&parse(kind, &body)), "{body}");
    }
}

#[test]
fn the_anthropic_usage_names_cache_creation_as_cache_write() {
    // inst-ad-usage-anthropic: reasoning is 0
    let mut body = anthropic_body(&json!([{ "type": "text", "text": "t" }]), "end_turn");
    body["usage"] = json!({ "input_tokens": 300, "output_tokens": 70,
                             "cache_read_input_tokens": 200, "cache_creation_input_tokens": 50 });
    assert_eq!(
        ok(ProviderKind::AnthropicMessages, &body).usage,
        usage(300, 70, 200, 50, 0)
    );
}

#[test]
fn absent_anthropic_counts_are_zero_and_an_absent_usage_object_is_all_zero() {
    // inst-ad-usage-match
    let kind = ProviderKind::AnthropicMessages;
    let blocks = json!([{ "type": "text", "text": "t" }]);
    let mut partial = anthropic_body(&blocks, "end_turn");
    partial["usage"] = json!({ "output_tokens": 4 });
    assert_eq!(ok(kind, &partial).usage, usage(0, 4, 0, 0, 0));
    assert_eq!(
        ok(kind, &anthropic_body(&blocks, "end_turn")).usage,
        UsageTokens::default()
    );
}

// ── every kind ───────────────────────────────────────────────────────────────

#[test]
fn bytes_that_are_not_a_json_object_are_malformed_for_every_kind() {
    // inst-ad-parse, inst-ad-malformed, inst-ad-return-malformed
    for kind in ALL_KINDS {
        for bytes in [
            &b""[..],
            b"not json",
            b"[1, 2]",
            b"\"text\"",
            b"null",
            b"42",
            b"{\"a\":",
        ] {
            assert!(
                is_malformed(&parse_completion(kind, bytes)),
                "{kind:?}: {}",
                String::from_utf8_lossy(bytes)
            );
        }
    }
}

#[test]
fn an_empty_or_whitespace_text_is_a_success_with_that_text() {
    // inst-ad-empty-text: not a call failure; the response parsing turns it into `empty_summary`
    let responses = json!({ "status": "completed", "output": [message_item(&["  \n "])] });
    assert_eq!(ok(ProviderKind::OpenaiResponses, &responses).text, "  \n ");
    let empty_output = json!({ "status": "completed", "output": [] });
    assert_eq!(ok(ProviderKind::OpenaiResponses, &empty_output).text, "");
    let anthropic = anthropic_body(&json!([]), "end_turn");
    assert_eq!(ok(ProviderKind::AnthropicMessages, &anthropic).text, "");
    let blank = anthropic_body(&json!([{ "type": "text", "text": "   " }]), "end_turn");
    assert_eq!(ok(ProviderKind::AnthropicMessages, &blank).text, "   ");
}

#[test]
fn the_usage_of_the_call_is_returned_with_the_text() {
    // inst-ad-return
    let completion = ok(
        ProviderKind::OpenaiResponses,
        &crate::test_support::maintenance::responses_answer("t", (11, 22)),
    );
    assert_eq!(completion.text, "t");
    assert_eq!(completion.usage, usage(11, 22, 0, 0, 0));
}

// ── classifying a non-2xx answer ─────────────────────────────────────────────

fn classify(status: u16, body: &Value) -> SummaryCallFailure {
    classify_error_response(status, body.to_string().as_bytes())
}

#[test]
fn an_openai_context_length_code_is_prompt_too_long() {
    // inst-ad-classify-ptl, inst-ad-return-ptl
    let body = json!({ "error": { "code": "context_length_exceeded", "message": "too big",
                                    "type": "invalid_request_error" } });
    assert_eq!(classify(400, &body), SummaryCallFailure::PromptTooLong);
    assert_eq!(classify(413, &body), SummaryCallFailure::PromptTooLong);
}

#[test]
fn the_openai_maximum_context_length_message_is_prompt_too_long() {
    // inst-ad-classify-ptl
    let body = json!({ "error": {
        "message": "This model's maximum context length is 128000 tokens. However, your messages resulted in 130001 tokens.",
        "type": "invalid_request_error", "param": "messages", "code": null } });
    assert_eq!(classify(400, &body), SummaryCallFailure::PromptTooLong);
}

#[test]
fn the_anthropic_prompt_is_too_long_message_is_prompt_too_long() {
    // inst-ad-classify-ptl
    let body = json!({ "type": "error", "error": { "type": "invalid_request_error",
        "message": "prompt is too long: 210000 tokens > 200000 maximum" } });
    assert_eq!(classify(400, &body), SummaryCallFailure::PromptTooLong);
    assert_eq!(classify(413, &body), SummaryCallFailure::PromptTooLong);
}

#[test]
fn a_vllm_bad_request_with_the_message_under_error_is_prompt_too_long() {
    // inst-ad-classify-ptl: the `error.type` value `BadRequestError` is not consulted
    let body = json!({ "error": {
        "message": "This model's maximum context length is 4096 tokens. However, you requested 5000 tokens.",
        "type": "BadRequestError", "param": null, "code": 400 } });
    assert_eq!(classify(400, &body), SummaryCallFailure::PromptTooLong);
}

#[test]
fn a_vllm_body_with_the_message_at_the_top_level_is_prompt_too_long() {
    // inst-ad-classify-ptl: older releases put the message at the top level
    let body = json!({ "object": "error",
        "message": "This model's maximum context length is 4096 tokens. However, you requested 5000 tokens.",
        "type": "BadRequestError", "param": null, "code": 400 });
    assert_eq!(classify(400, &body), SummaryCallFailure::PromptTooLong);
}

#[test]
fn a_top_level_message_is_read_when_error_has_no_message() {
    // inst-ad-classify-ptl: `error` present without a message
    let body = json!({ "error": { "type": "BadRequestError" },
                        "message": "prompt is too long for this model" });
    assert_eq!(classify(400, &body), SummaryCallFailure::PromptTooLong);
}

#[test]
fn the_wording_is_compared_case_insensitively() {
    // inst-ad-classify-ptl
    for message in [
        "MAXIMUM CONTEXT LENGTH exceeded",
        "Prompt Is Too Long",
        "error: CONTEXT_LENGTH_EXCEEDED while reading",
    ] {
        let body = json!({ "error": { "message": message } });
        assert_eq!(
            classify(400, &body),
            SummaryCallFailure::PromptTooLong,
            "{message}"
        );
    }
}

#[test]
fn a_context_length_code_is_matched_only_for_400_and_413() {
    // inst-ad-classify-ptl: any other status is a plain failure
    let body = json!({ "error": { "code": "context_length_exceeded",
                                    "message": "maximum context length" } });
    for status in [401, 404, 422, 429, 500, 502, 503] {
        assert!(
            matches!(classify(status, &body), SummaryCallFailure::Other { .. }),
            "{status}"
        );
    }
}

#[test]
fn a_400_with_an_unrelated_message_is_not_prompt_too_long() {
    // inst-ad-classify-ptl, inst-ad-return-other: `invalid_request_error` alone is not enough
    let body = json!({ "error": { "type": "invalid_request_error",
                                    "message": "Unsupported parameter: 'foo'." } });
    assert!(matches!(
        classify(400, &body),
        SummaryCallFailure::Other { .. }
    ));
}

#[test]
fn the_error_type_value_is_never_consulted() {
    // inst-ad-classify-ptl: a matching type with an unrelated message is not classified
    let body = json!({ "error": { "type": "context_length_exceeded", "message": "nope" } });
    assert!(matches!(
        classify(400, &body),
        SummaryCallFailure::Other { .. }
    ));
}

#[test]
fn a_request_too_large_answer_is_not_a_context_length_rejection() {
    // inst-ad-classify-ptl: a byte-size rejection is a plain failure
    let body = json!({ "type": "error", "error": { "type": "request_too_large",
        "message": "Request exceeds the maximum allowed number of bytes." } });
    assert!(matches!(
        classify(413, &body),
        SummaryCallFailure::Other { .. }
    ));
}

#[test]
fn a_body_that_is_not_json_is_a_plain_failure() {
    // inst-ad-read-error, inst-ad-return-other
    for bytes in [
        &b""[..],
        b"<html>502 Bad Gateway</html>",
        b"[]",
        b"\"maximum context length\"",
    ] {
        assert!(
            matches!(
                classify_error_response(400, bytes),
                SummaryCallFailure::Other { .. }
            ),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn a_429_is_a_failure_like_any_other_status() {
    // inst-ad-return-other: retried only through the summary retry budget
    let body = json!({ "error": { "message": "Rate limit reached", "type": "rate_limit_error" } });
    let SummaryCallFailure::Other { cause } = classify(429, &body) else {
        panic!("a 429 is not prompt too long");
    };
    assert!(
        cause.contains("429"),
        "the status code is in the cause: {cause}"
    );
}

#[test]
fn the_cause_holds_no_provider_text_url_or_identifier() {
    // inst-ad-return-other, inst-ad-no-content-logs: sanitized
    let body = json!({ "error": {
        "message": "PROVIDER-SECRET-TEXT see https://internal.provider.example/v1/x for org-1234567",
        "type": "server_error", "param": "req_abcdef" } });
    for status in [400, 403, 500, 503] {
        let SummaryCallFailure::Other { cause } = classify(status, &body) else {
            panic!("{status}");
        };
        for leaked in [
            "PROVIDER-SECRET-TEXT",
            "https://",
            "internal.provider",
            "org-1234567",
            "req_abcdef",
        ] {
            assert!(!cause.contains(leaked), "{status}: {cause}");
        }
        assert!(cause.contains(&status.to_string()), "{cause}");
    }
}
