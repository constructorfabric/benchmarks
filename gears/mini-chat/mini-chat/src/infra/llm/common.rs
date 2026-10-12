//! Helpers shared by the adapters.

use mini_chat_sdk::TokenUsage;
use serde_json::{Map, Value};
use tracing::warn;

use crate::domain::llm::ApiParams;

/// Keys of the request body the request controls: `extra_body` cannot override them.
const RESERVED_KEYS: [&str; 18] = [
    "model",
    "input",
    "messages",
    "instructions",
    "system",
    "stream",
    "stream_options",
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
    "max_tool_calls",
    "tools",
    "tool_choice",
    "include",
    "store",
    "previous_response_id",
    "user",
    "metadata",
];

/// Merges the `extra_body` keys into the top level of the body; reserved keys are ignored with a
/// warning.
pub fn merge_extra_body(body: &mut Map<String, Value>, params: &ApiParams) {
    let Some(extra) = &params.extra_body else {
        return;
    };
    for (key, value) in extra {
        if RESERVED_KEYS.contains(&key.as_str()) {
            warn!(key = %key, "extra_body key is controlled by the request and was ignored");
        } else {
            body.insert(key.clone(), value.clone());
        }
    }
}

/// Inserts the sampling parameters that are set.
pub fn insert_sampling(body: &mut Map<String, Value>, params: &ApiParams) {
    for (key, value) in [
        ("temperature", params.temperature),
        ("top_p", params.top_p),
        ("frequency_penalty", params.frequency_penalty),
        ("presence_penalty", params.presence_penalty),
    ] {
        if let Some(value) = value {
            body.insert(key.to_owned(), Value::from(value));
        }
    }
}

fn count(value: Option<&Value>) -> i64 {
    value.and_then(Value::as_i64).unwrap_or(0).max(0)
}

/// Reads a usage object with the field names of the given API.
///
/// `input`/`output` are the field names of the totals; the details objects are
/// `{input_details}.cached_tokens` and `{output_details}.reasoning_tokens`.
pub fn parse_usage(
    usage: &Value,
    input: &str,
    output: &str,
    input_details: &str,
    output_details: &str,
) -> TokenUsage {
    TokenUsage {
        input_tokens: count(usage.get(input)),
        output_tokens: count(usage.get(output)),
        cache_read_input_tokens: count(
            usage
                .get(input_details)
                .and_then(|d| d.get("cached_tokens")),
        ),
        cache_write_input_tokens: 0,
        reasoning_tokens: count(
            usage
                .get(output_details)
                .and_then(|d| d.get("reasoning_tokens")),
        ),
    }
}

/// The message of a provider error body: `error.message`, `message`, or the raw text.
#[must_use]
pub fn error_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    match serde_json::from_slice::<Value>(body) {
        Ok(json) => json
            .pointer("/error/message")
            .or_else(|| json.get("message"))
            .or_else(|| json.get("error").filter(|error| error.is_string()))
            .and_then(Value::as_str)
            .map_or_else(|| text.trim().to_owned(), str::to_owned),
        Err(_) => text.trim().to_owned(),
    }
}

/// Normalizes a provider's incomplete reason to `max_tokens`, `content_filter` or `other`.
#[must_use]
pub fn normalize_incomplete_reason(reason: Option<&str>) -> String {
    match reason {
        Some("max_output_tokens" | "max_tokens" | "length") => "max_tokens",
        Some("content_filter") => "content_filter",
        _ => "other",
    }
    .to_owned()
}

/// The text of a `response.failed` / `error` payload: `response.error`, then a top-level `error`,
/// then flat `{code, message}`; unparseable data becomes the message itself.
pub fn failure_message(data: &str) -> String {
    let Ok(json) = serde_json::from_str::<Value>(data) else {
        return data.trim().to_owned();
    };
    let text = |value: Option<&Value>| {
        value
            .and_then(|error| error.get("message").or(Some(error)))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    text(json.pointer("/response/error"))
        .or_else(|| text(json.get("error")))
        .or_else(|| {
            json.get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "The provider reported a failure".to_owned())
}
