//! JSON helpers shared by the adapters.

use serde_json::{Map, Value};

use crate::infra::llm::types::LlmRequest;

/// Request keys the adapters control; `extra_body` cannot override them
/// (DESIGN section 4 "Model catalog", `general_config.api_params`).
const CONTROLLED_KEYS: &[&str] = &[
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

/// Merge `extra_body` into the top level: controlled keys are ignored with a
/// warning, keys the adapter already set are kept.
pub(super) fn merge_extra_body(body: &mut Map<String, Value>, req: &LlmRequest) {
    let Some(extra) = &req.api_params.extra_body else {
        return;
    };
    for (key, value) in extra {
        if CONTROLLED_KEYS.contains(&key.as_str()) {
            tracing::warn!(key = %key, model = %req.model, "extra_body key controlled by the request; ignored");
            continue;
        }
        body.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

/// The array at `v` (empty when absent or not an array).
pub(super) fn array(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// The string field `key` of `v` (empty when absent).
pub(super) fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The integer field `key` of `obj` (0 when absent).
pub(super) fn int_field(obj: Option<&Value>, key: &str) -> i64 {
    obj.and_then(|o| o.get(key))
        .and_then(Value::as_i64)
        .unwrap_or(0)
}
