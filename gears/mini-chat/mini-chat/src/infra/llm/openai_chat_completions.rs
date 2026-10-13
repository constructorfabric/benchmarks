//! `openai_chat_completions` adapter: Chat Completions API (DESIGN section
//! 3.2 `llm_provider`, section 3.3 `event: tool`, "Provider Event
//! Translation", "Provider Request Metadata", "Model Catalog Configuration",
//! section 5.4 "Canonical provider usage fields", ADR-0005).
//!
//! **Request.** `messages` holds the system instructions (a `system`
//! message, omitted when empty), the conversation and the current user
//! message (plain string contents). Image inputs are dropped: the protocol
//! cannot reference a provider file id for an image. Function calls of an
//! earlier agentic iteration become `tool_calls` of an assistant message
//! (joined to the preceding assistant text), their outputs `tool` messages. The body carries
//! `model`, `stream` (with `stream_options.include_usage` when streaming),
//! `max_completion_tokens`, `user`, the catalog sampling parameters, `stop`,
//! `reasoning_effort` and the merged `extra_body` (keys the request controls
//! are ignored with a warning). Only function tools are sent; `file_search`,
//! `web_search` and `code_interpreter` are dropped. No `metadata` and no
//! `max_tool_calls`.
//!
//! **Stream.** `data:` chunks (no `event:` names) until `data: [DONE]`:
//!
//! | Wire | Internal event |
//! |---|---|
//! | `choices[0].delta.content` | text delta |
//! | first `choices[0].delta.tool_calls[i]` of an index | `tool` start `function_call` (`details: {index, call_id, name}`) |
//! | `choices[0].finish_reason` with tool calls | per call: `tool` done `function_call` (`details: {call_id, name, arguments}`) and the function call |
//! | `usage` (final chunk) | usage: `prompt_tokens` → input, `completion_tokens` → output, `prompt_tokens_details.cached_tokens` → cache read, `completion_tokens_details.reasoning_tokens` → reasoning |
//! | `[DONE]`, or the end of the body after a `finish_reason` | completed; `finish_reason = length` is incomplete (`max_tokens`), `content_filter` is incomplete (`content_filter`) |
//! | `{"error": {...}}` | `provider_error` with the sanitized provider message |
//!
//! HTTP and gateway errors are mapped like the `OpenAI` Responses adapter.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::ModelApiParams;
use oagw_sdk::api::ServiceGatewayClientV1;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::openai_responses::{
    error_fields, merge_extra_body, sse_error_failure, string_field, tool_completed,
};
use super::sse::{STREAM_ENDED_MESSAGE, SseEvent, SseTranslator, event_stream};
use super::transport::{
    CANCELLED_MESSAGE, COMPLETION_BODY_LIMIT, INVALID_RESPONSE_MESSAGE, ProviderTransport,
    read_limited,
};
use crate::domain::llm::{
    LlmCompletion, LlmEvent, LlmEventStream, LlmFailure, LlmFunctionCall, LlmInputItem,
    LlmProvider, LlmRequest, LlmTool, LlmUsage, ProviderTarget,
};
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::oagw_provisioning::SystemContextSource;

/// `event: tool` name of a Chat Completions function tool call.
pub const TOOL_FUNCTION_CALL: &str = "function_call";

/// Incomplete reason of a response cut at the output limit.
pub const INCOMPLETE_MAX_TOKENS: &str = "max_tokens";
/// Incomplete reason of a response stopped by content filtering.
pub const INCOMPLETE_CONTENT_FILTER: &str = "content_filter";

/// End-of-stream sentinel of the `data:` lines.
const DONE_SENTINEL: &str = "[DONE]";

/// The `openai_chat_completions` adapter.
#[derive(Debug)]
pub struct OpenAiChatCompletionsAdapter {
    transport: ProviderTransport,
}

impl OpenAiChatCompletionsAdapter {
    /// Adapter over the OAGW client and the S2S context source.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        system: Arc<dyn SystemContextSource>,
    ) -> Self {
        Self {
            transport: ProviderTransport::new(gateway, system),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiChatCompletionsAdapter {
    async fn stream(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmEventStream, LlmFailure> {
        let body = build_request_body(&req, true);
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LlmFailure::provider_error(CANCELLED_MESSAGE)),
            response = self.transport.send(target, &req.model, &body, true, &[]) => response?,
        };
        let body = response.into_body().into_stream();
        Ok(event_stream(
            body,
            ChatCompletionsTranslator::default(),
            cancel,
        ))
    }

    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, LlmFailure> {
        let body = build_request_body(&req, false);
        let response = self
            .transport
            .send(target, &req.model, &body, false, &[])
            .await?;
        let body = read_limited(response.into_body(), COMPLETION_BODY_LIMIT).await;
        parse_completion(&body)
    }
}

// ── Request ──────────────────────────────────────────────────────────────

/// Chat Completions request body (module docs).
#[must_use]
pub fn build_request_body(req: &LlmRequest, stream: bool) -> Value {
    let mut body = Map::new();
    merge_extra_body(&mut body, req.api_params.extra_body.as_ref());
    insert_api_params(&mut body, &req.api_params);

    body.insert("model".to_owned(), json!(req.model));
    body.insert("messages".to_owned(), Value::Array(messages(req)));
    body.insert("stream".to_owned(), json!(stream));
    if stream {
        body.insert(
            "stream_options".to_owned(),
            json!({ "include_usage": true }),
        );
    }
    body.insert(
        "max_completion_tokens".to_owned(),
        json!(req.max_output_tokens),
    );
    let tools: Vec<Value> = req.tools.iter().filter_map(tool_json).collect();
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    body.insert("user".to_owned(), json!(req.user));
    Value::Object(body)
}

fn insert_api_params(body: &mut Map<String, Value>, params: &ModelApiParams) {
    let sampling = [
        ("temperature", params.temperature),
        ("top_p", params.top_p),
        ("frequency_penalty", params.frequency_penalty),
        ("presence_penalty", params.presence_penalty),
    ];
    for (key, value) in sampling {
        if let Some(value) = value {
            body.insert(key.to_owned(), json!(value));
        }
    }
    if !params.stop.is_empty() {
        body.insert("stop".to_owned(), json!(params.stop));
    }
    if let Some(effort) = params.reasoning_effort.as_deref() {
        body.insert("reasoning_effort".to_owned(), json!(effort));
    }
}

fn messages(req: &LlmRequest) -> Vec<Value> {
    let mut messages = Vec::with_capacity(req.input.len() + 1);
    if !req.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": req.instructions }));
    }
    for item in &req.input {
        let message = match item {
            LlmInputItem::User {
                text,
                image_file_ids,
            } => {
                if !image_file_ids.is_empty() {
                    debug!(
                        target: "mini_chat.llm",
                        images = image_file_ids.len(),
                        "Chat Completions cannot reference provider image files; images dropped"
                    );
                }
                json!({ "role": "user", "content": text })
            }
            LlmInputItem::Assistant { text } => json!({ "role": "assistant", "content": text }),
            LlmInputItem::FunctionCall(call) => {
                let tool_call = json!({
                    "id": call.call_id,
                    "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments },
                });
                // Calls of one response join the preceding assistant message.
                if let Some(calls) = messages
                    .last_mut()
                    .filter(|last| last.get("role").and_then(Value::as_str) == Some("assistant"))
                    .and_then(Value::as_object_mut)
                    .map(|last| last.entry("tool_calls").or_insert_with(|| json!([])))
                    .and_then(Value::as_array_mut)
                {
                    calls.push(tool_call);
                    continue;
                }
                json!({ "role": "assistant", "content": null, "tool_calls": [tool_call] })
            }
            LlmInputItem::FunctionCallOutput { call_id, output } => {
                json!({ "role": "tool", "tool_call_id": call_id, "content": output })
            }
        };
        messages.push(message);
    }
    messages
}

/// Function tools only; built-in tools are dropped.
fn tool_json(tool: &LlmTool) -> Option<Value> {
    match tool {
        LlmTool::Function(function) => Some(json!({
            "type": "function",
            "function": {
                "name": function.name,
                "description": function.description,
                "parameters": function.parameters,
            },
        })),
        LlmTool::FileSearch { .. }
        | LlmTool::WebSearch { .. }
        | LlmTool::CodeInterpreter { .. } => None,
    }
}

// ── Usage, finish reasons, completion ────────────────────────────────────

/// Normalized usage of a Chat Completions `usage` object; `None` when absent.
#[must_use]
pub fn parse_usage(value: &Value) -> Option<LlmUsage> {
    let usage = value.get("usage").filter(|usage| usage.is_object())?;
    let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_i64).unwrap_or(0);
    Some(LlmUsage {
        input_tokens: count("/prompt_tokens"),
        output_tokens: count("/completion_tokens"),
        cache_read_input_tokens: count("/prompt_tokens_details/cached_tokens"),
        cache_write_input_tokens: 0,
        reasoning_tokens: count("/completion_tokens_details/reasoning_tokens"),
    })
}

/// Incomplete reason of a `finish_reason`; `None` for a complete response.
#[must_use]
pub fn incomplete_reason(finish_reason: &str) -> Option<String> {
    match finish_reason {
        "length" => Some(INCOMPLETE_MAX_TOKENS.to_owned()),
        "content_filter" => Some(INCOMPLETE_CONTENT_FILTER.to_owned()),
        _ => None,
    }
}

/// Result of a non-streaming response body.
///
/// # Errors
///
/// `provider_error` for a body that is not a completion or carries an error.
pub fn parse_completion(body: &[u8]) -> Result<LlmCompletion, LlmFailure> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| LlmFailure::provider_error(INVALID_RESPONSE_MESSAGE))?;
    if value.get("error").is_some_and(|error| !error.is_null()) {
        let (message, code) =
            error_fields(&value).unwrap_or_else(|| (INVALID_RESPONSE_MESSAGE.to_owned(), None));
        return Err(LlmFailure {
            provider_code: code,
            ..LlmFailure::provider_error(sanitize_provider_message(&message))
        });
    }
    let choice = value
        .pointer("/choices/0")
        .ok_or_else(|| LlmFailure::provider_error(INVALID_RESPONSE_MESSAGE))?;
    let text = choice
        .pointer("/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(LlmCompletion {
        text,
        usage: parse_usage(&value),
        response_id: string_field(&value, "id"),
        incomplete_reason: incomplete_reason(finish),
    })
}

// ── Stream translation ───────────────────────────────────────────────────

/// One streamed tool call, assembled from its deltas.
#[derive(Debug, Default)]
struct PendingCall {
    call_id: String,
    name: String,
    arguments: String,
}

/// Chat Completions stream translation (module docs).
#[derive(Debug, Default)]
pub struct ChatCompletionsTranslator {
    response_id: Option<String>,
    text: String,
    usage: Option<LlmUsage>,
    finish_reason: Option<String>,
    /// Tool calls by `index`.
    calls: BTreeMap<u64, PendingCall>,
}

impl SseTranslator for ChatCompletionsTranslator {
    fn translate(&mut self, event: SseEvent, out: &mut Vec<LlmEvent>) {
        let raw = event.data.trim();
        if raw == DONE_SENTINEL {
            self.finish_calls(out);
            out.push(self.completed());
            return;
        }
        let data: Option<Value> = serde_json::from_str(raw).ok();
        let is_error = event.event.as_deref() == Some("error")
            || data
                .as_ref()
                .and_then(|data| data.get("error"))
                .is_some_and(|error| !error.is_null());
        if is_error {
            out.push(LlmEvent::Failed(sse_error_failure(data.as_ref(), raw)));
            return;
        }
        if let Some(data) = data {
            self.on_chunk(&data, out);
        }
    }

    fn stream_ended(&mut self) -> LlmEvent {
        if self.finish_reason.is_some() {
            self.completed()
        } else {
            LlmEvent::Failed(LlmFailure::provider_error(STREAM_ENDED_MESSAGE))
        }
    }
}

impl ChatCompletionsTranslator {
    fn on_chunk(&mut self, data: &Value, out: &mut Vec<LlmEvent>) {
        if self.response_id.is_none() {
            self.response_id = string_field(data, "id");
        }
        if let Some(usage) = parse_usage(data) {
            self.usage = Some(usage);
        }
        let Some(choice) = data.pointer("/choices/0") else {
            return;
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                self.text.push_str(content);
                out.push(LlmEvent::TextDelta(content.to_owned()));
            }
            for call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.on_tool_call_delta(call, out);
            }
        }
        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str)
            && self.finish_reason.is_none()
        {
            self.finish_reason = Some(finish.to_owned());
            self.finish_calls(out);
        }
    }

    fn on_tool_call_delta(&mut self, call: &Value, out: &mut Vec<LlmEvent>) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let started = self.calls.contains_key(&index);
        let pending = self.calls.entry(index).or_default();
        if let Some(call_id) = call.get("id").and_then(Value::as_str) {
            call_id.clone_into(&mut pending.call_id);
        }
        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
            pending.name.push_str(name);
        }
        if let Some(arguments) = call.pointer("/function/arguments").and_then(Value::as_str) {
            pending.arguments.push_str(arguments);
        }
        if !started {
            out.push(LlmEvent::ToolStarted {
                name: TOOL_FUNCTION_CALL.to_owned(),
                details: json!({
                    "index": index,
                    "call_id": pending.call_id,
                    "name": pending.name,
                }),
            });
        }
    }

    /// Reports every assembled tool call (at the finish reason).
    fn finish_calls(&mut self, out: &mut Vec<LlmEvent>) {
        for call in std::mem::take(&mut self.calls).into_values() {
            out.push(tool_completed(
                TOOL_FUNCTION_CALL,
                json!({
                    "call_id": call.call_id,
                    "name": call.name,
                    "arguments": call.arguments,
                }),
            ));
            out.push(LlmEvent::FunctionCall(LlmFunctionCall {
                call_id: call.call_id,
                name: call.name,
                arguments: call.arguments,
            }));
        }
    }

    fn completed(&mut self) -> LlmEvent {
        LlmEvent::Completed {
            response_id: self.response_id.take(),
            usage: self.usage,
            incomplete_reason: self.finish_reason.as_deref().and_then(incomplete_reason),
            output_text: std::mem::take(&mut self.text),
        }
    }
}

#[cfg(test)]
#[path = "openai_chat_completions_tests.rs"]
mod tests;
