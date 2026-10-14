//! The reading of the non-streaming summary answer per adapter kind
//! (`cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call`, steps `inst-ad-non-2xx`,
//! `inst-ad-parse`, `inst-ad-extract-match`, `inst-ad-usage-match` and `inst-ad-empty-text`): the
//! text and the usage of a 2xx body, and the classification of an error answer.

use mini_chat_sdk::UsageTokens;
use serde_json::Value;
use toolkit_macros::domain_model;

use crate::config::ProviderKind;

/// The text and the usage of a successful call.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryCompletion {
    /// The extracted text; empty or whitespace-only is still a success.
    pub text: String,
    pub usage: UsageTokens,
}

/// Why the summary call failed.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryCallFailure {
    /// The provider rejected the prompt as longer than the model's context length.
    PromptTooLong,
    /// Any other failure; `cause` is sanitized: no provider identifier, URL, credential or body
    /// text.
    Other { cause: String },
}

/// Classify a non-2xx answer: `PromptTooLong` for a 400 or 413 whose `error.code` is
/// `context_length_exceeded` or whose message (`error.message`, or a top-level `message` when
/// `error` is absent or has no message) contains `maximum context length`, `prompt is too long`
/// or `context_length_exceeded` (case-insensitive); `error.type` is not consulted. Any other
/// answer is `Other` with the status code in the cause.
#[must_use]
pub fn classify_error_response(status: u16, body: &[u8]) -> SummaryCallFailure {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-classify-ptl
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-ptl
    if matches!(status, 400 | 413) && is_context_length_rejection(body) {
        return SummaryCallFailure::PromptTooLong;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-ptl
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-classify-ptl
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-other
    // the cause holds the status code only: no provider identifier, URL, credential or body text;
    // a 429 is a failure like any other status and is retried only through the retry budget
    SummaryCallFailure::Other {
        cause: format!("the provider answered with status {status}"),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-other
}

/// Whether an error body is a context-length rejection: `error.code` or a known wording of the
/// message (`error.message`, else a top-level `message`); `error.type` is never consulted.
fn is_context_length_rejection(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let error = value.get("error");
    if error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        == Some("context_length_exceeded")
    {
        return true;
    }
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str));
    message.is_some_and(|message| {
        let lower = message.to_lowercase();
        [
            "maximum context length",
            "prompt is too long",
            "context_length_exceeded",
        ]
        .iter()
        .any(|wording| lower.contains(wording))
    })
}

const MALFORMED: &str = "the provider response is malformed";

fn malformed() -> SummaryCallFailure {
    SummaryCallFailure::Other {
        cause: MALFORMED.to_owned(),
    }
}

/// The count at `path`; an empty path (the provider has no such count) and an absent or non-numeric
/// field are 0.
fn count(value: &Value, path: &[&str]) -> u64 {
    if path.is_empty() {
        return 0;
    }
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Remove every `<think>...</think>` block.
fn strip_think(text: &str) -> String {
    let mut rest = text;
    let mut out = String::new();
    while let Some(open) = rest.find("<think>") {
        let Some(close) = rest[open..].find("</think>") else {
            break;
        };
        out.push_str(&rest[..open]);
        rest = &rest[open + close + "</think>".len()..];
    }
    out.push_str(rest);
    out
}

/// The text and the usage of one kind's body.
type Extracted = (String, UsageTokens);

/// The usage of a body: every count that is absent is 0, an absent usage object is all 0.
fn usage_of(value: &Value, counts: [&[&str]; 5]) -> UsageTokens {
    let [input, output, cache_read, cache_write, reasoning] = counts;
    UsageTokens {
        input_tokens: count(value, input),
        output_tokens: count(value, output),
        cache_read_input_tokens: count(value, cache_read),
        cache_write_input_tokens: count(value, cache_write),
        reasoning_tokens: count(value, reasoning),
    }
}

/// `openai_responses` and `vllm_responses`.
fn read_responses(value: &Value, vllm: bool) -> Result<Extracted, SummaryCallFailure> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-text-responses
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    if value.get("status").and_then(Value::as_str) == Some("failed")
        || value.get("error").is_some_and(|error| !error.is_null())
    {
        return Err(SummaryCallFailure::Other {
            cause: "the provider reported a failed response".to_owned(),
        });
    }
    // the `output_text` parts of the `message` items are joined in order without a separator;
    // `reasoning` and every other item are ignored, and an `incomplete` status is a truncated but
    // valid completion
    let mut text = String::new();
    for item in output
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
    {
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        {
            text.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
        }
    }
    if vllm {
        // the streaming adapter treats this text as reasoning
        text = strip_think(&text);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-text-responses
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-usage-responses
    let usage = usage_of(
        value,
        [
            &["usage", "input_tokens"],
            &["usage", "output_tokens"],
            &["usage", "input_tokens_details", "cached_tokens"],
            &[],
            &["usage", "output_tokens_details", "reasoning_tokens"],
        ],
    );
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-usage-responses
    Ok((text, usage))
}

/// `openai_chat_completions`.
fn read_chat_completions(value: &Value) -> Result<Extracted, SummaryCallFailure> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-text-chat-completions
    let first = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(malformed)?;
    // a null or absent content is an empty text; a `finish_reason` of `length` is a truncated
    // but valid completion
    let text = first
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-text-chat-completions
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-usage-chat-completions
    let usage = usage_of(
        value,
        [
            &["usage", "prompt_tokens"],
            &["usage", "completion_tokens"],
            &["usage", "prompt_tokens_details", "cached_tokens"],
            &[],
            &["usage", "completion_tokens_details", "reasoning_tokens"],
        ],
    );
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-usage-chat-completions
    Ok((text, usage))
}

/// `anthropic_messages`.
fn read_anthropic(value: &Value) -> Result<Extracted, SummaryCallFailure> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-text-anthropic
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return Err(malformed());
    }
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(malformed)?;
    // the `text` blocks are joined in order; `thinking`, `tool_use` and every other block are
    // ignored, and a `stop_reason` of `max_tokens` is a truncated but valid completion
    let text: String = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-text-anthropic
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-usage-anthropic
    let usage = usage_of(
        value,
        [
            &["usage", "input_tokens"],
            &["usage", "output_tokens"],
            &["usage", "cache_read_input_tokens"],
            &["usage", "cache_creation_input_tokens"],
            &[],
        ],
    );
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-usage-anthropic
    Ok((text, usage))
}

/// Read the 2xx body of `kind`: the text per the kind's extraction rules (the text parts are
/// concatenated in order without a separator) and the usage per the kind's usage fields.
///
/// # Errors
///
/// `Other` with the cause `the provider response is malformed` for a body that is not a JSON
/// object or lacks the structure of its kind, and `Other` for a Responses `status` of `failed`
/// or a non-null `error`.
pub fn parse_completion(
    kind: ProviderKind,
    body: &[u8],
) -> Result<SummaryCompletion, SummaryCallFailure> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-parse
    let parsed = serde_json::from_slice::<Value>(body);
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-parse
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-malformed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-malformed
    let value = parsed.ok().filter(Value::is_object).ok_or_else(malformed)?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return-malformed
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-malformed
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-extract-match
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-usage-match
    let (text, usage) = match kind {
        ProviderKind::OpenaiResponses => read_responses(&value, false)?,
        ProviderKind::VllmResponses => read_responses(&value, true)?,
        ProviderKind::OpenaiChatCompletions => read_chat_completions(&value)?,
        ProviderKind::AnthropicMessages => read_anthropic(&value)?,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-usage-match
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-extract-match
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-empty-text
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return
    // an empty or whitespace-only text is a success: the response parsing turns it into the
    // `empty_summary` outcome
    Ok(SummaryCompletion { text, usage })
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-return
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call:p1:inst-ad-empty-text
}

#[cfg(test)]
#[path = "summary_response_tests.rs"]
mod summary_response_tests;
