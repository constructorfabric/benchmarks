//! OpenAI-compatible Chat Completions adapter (`/v1/chat/completions`;
//! DESIGN section 3.2 `llm_provider`, section 3.3 tool events, section 4
//! "Provider Request Metadata").
//!
//! - The system prompt is the first `system` message; text parts are sent as
//!   string content. Image parts are dropped: the protocol has no way to
//!   reference an uploaded provider file as an image.
//! - `file_search`, `web_search` and `code_interpreter` are dropped; function
//!   tools are kept. `max_tool_calls` and `metadata` are not sent; `user` is.
//! - Streaming uses `stream_options.include_usage`; the usage chunk comes after
//!   the `finish_reason` chunk, so `Completed` is emitted at `data: [DONE]`
//!   (or at the end of the body once a `finish_reason` was seen).
//! - Function tool calls are reported as `tool` events named `function_call`
//!   (`start` with `index`, `call_id`, `name`; `done` with `call_id`, `name`,
//!   `arguments`) and as [`LlmEvent::FunctionCall`] at `finish_reason`.
//! - `finish_reason: "length"` is an incomplete completion with reason
//!   `max_tokens`; `content_filter` keeps its name.

use std::collections::BTreeMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::error_from_value;
use super::stream::Translate;
use super::wire::{array, int_field, merge_extra_body, str_field};
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::llm::sse::SseFrame;
use crate::infra::llm::types::{
    CompletionResult, ContentPart, InputItem, LlmEvent, LlmRequest, ProviderError, ToolSpec,
};

/// `tool` event name of function tool calls.
const FUNCTION_CALL: &str = "function_call";

// ── Request ──────────────────────────────────────────────────────────────────

/// The Chat Completions request body.
pub(super) fn build_body(req: &LlmRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    body.insert("messages".to_owned(), Value::Array(messages(req)));
    let tools: Vec<Value> = req.tools.iter().filter_map(tool).collect();
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    body.insert(
        "max_completion_tokens".to_owned(),
        json!(req.max_output_tokens),
    );
    let p = &req.api_params;
    for (key, value) in [
        ("temperature", p.temperature),
        ("top_p", p.top_p),
        ("frequency_penalty", p.frequency_penalty),
        ("presence_penalty", p.presence_penalty),
    ] {
        if let Some(v) = value {
            body.insert(key.to_owned(), json!(v));
        }
    }
    if !p.stop.is_empty() {
        body.insert("stop".to_owned(), json!(p.stop));
    }
    if let Some(effort) = &p.reasoning_effort {
        body.insert("reasoning_effort".to_owned(), json!(effort));
    }
    body.insert("user".to_owned(), json!(req.user));
    body.insert("stream".to_owned(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".to_owned(), json!({"include_usage": true}));
    }
    merge_extra_body(&mut body, req);
    Value::Object(body)
}

/// The system prompt, then the input items. Consecutive function calls form
/// one assistant message with `tool_calls`; each output is a `tool` message.
fn messages(req: &LlmRequest) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.input.len() + 1);
    if !req.instructions.is_empty() {
        out.push(json!({"role": "system", "content": req.instructions}));
    }
    for item in &req.input {
        match item {
            InputItem::Message { role, content } => {
                let text = message_text(content);
                if !text.is_empty() {
                    out.push(json!({"role": role, "content": text}));
                }
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let call = json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                });
                let open = out.last_mut().filter(|m| {
                    m.get("role").and_then(Value::as_str) == Some("assistant")
                        && m.get("tool_calls").is_some()
                });
                match open.and_then(|m| m.get_mut("tool_calls")?.as_array_mut()) {
                    Some(calls) => calls.push(call),
                    None => out
                        .push(json!({"role": "assistant", "content": null, "tool_calls": [call]})),
                }
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                out.push(json!({"role": "tool", "tool_call_id": call_id, "content": output}));
            }
        }
    }
    out
}

/// Text parts joined with `\n`; image parts are dropped.
fn message_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::InputText(t) | ContentPart::OutputText(t) => Some(t.as_str()),
            ContentPart::InputImage { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Function tools only; built-in tools are dropped.
fn tool(spec: &ToolSpec) -> Option<Value> {
    match spec {
        ToolSpec::Function {
            name,
            description,
            parameters,
        } => Some(json!({
            "type": "function",
            "function": {"name": name, "description": description, "parameters": parameters},
        })),
        ToolSpec::FileSearch { .. }
        | ToolSpec::WebSearch { .. }
        | ToolSpec::CodeInterpreter { .. } => None,
    }
}

// ── Responses ────────────────────────────────────────────────────────────────

fn parse_usage(v: Option<&Value>) -> Option<UsageTokens> {
    let u = v.filter(|u| u.is_object())?;
    Some(UsageTokens {
        input_tokens: int_field(Some(u), "prompt_tokens"),
        output_tokens: int_field(Some(u), "completion_tokens"),
        cache_read_input_tokens: int_field(u.get("prompt_tokens_details"), "cached_tokens"),
        cache_write_input_tokens: 0,
        reasoning_tokens: int_field(u.get("completion_tokens_details"), "reasoning_tokens"),
    })
}

/// `length` -> `max_tokens`; `content_filter` kept; others are complete.
fn incomplete_reason(finish_reason: Option<&str>) -> Option<String> {
    match finish_reason? {
        "length" => Some("max_tokens".to_owned()),
        "content_filter" => Some("content_filter".to_owned()),
        _ => None,
    }
}

fn top_level_error(v: &Value) -> Option<ProviderError> {
    let e = v.get("error").filter(|e| !e.is_null())?;
    Some(error_from_value(Some(e)).unwrap_or_else(|| {
        ProviderError::provider(
            e.as_str()
                .map_or_else(|| "provider error".to_owned(), sanitize_provider_message),
        )
    }))
}

/// Text and usage of a non-streaming reply.
pub(super) fn parse_completion(bytes: &[u8]) -> Result<CompletionResult, ProviderError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|_| ProviderError::provider("invalid provider response"))?;
    if let Some(e) = top_level_error(&v) {
        return Err(e);
    }
    let text = array(v.get("choices"))
        .first()
        .and_then(|c| c.get("message"))
        .map(|m| str_field(m, "content").to_owned())
        .unwrap_or_default();
    Ok(CompletionResult {
        text,
        usage: parse_usage(v.get("usage")),
    })
}

// ── Streaming ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

/// Per-stream translation state.
#[derive(Default)]
pub(super) struct Translator {
    response_id: Option<String>,
    /// Tool calls being streamed, by `index`.
    calls: BTreeMap<u64, PendingCall>,
    finish_reason: Option<String>,
    usage: Option<UsageTokens>,
}

impl Translate for Translator {
    fn on_frame(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        let data = frame.data.trim();
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            return vec![self.completed()];
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            tracing::debug!("unparseable provider chunk ignored");
            return Vec::new();
        };
        if let Some(error) = top_level_error(&v) {
            return vec![LlmEvent::Failed { error, usage: None }];
        }
        if self.response_id.is_none() {
            self.response_id = v
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
        }
        if let Some(usage) = parse_usage(v.get("usage")) {
            self.usage = Some(usage);
        }
        let mut events = Vec::new();
        for choice in array(v.get("choices")) {
            self.on_choice(choice, &mut events);
        }
        events
    }

    fn on_end(&mut self) -> Vec<LlmEvent> {
        if self.finish_reason.is_some() {
            vec![self.completed()]
        } else {
            Vec::new()
        }
    }
}

impl Translator {
    fn on_choice(&mut self, choice: &Value, events: &mut Vec<LlmEvent>) {
        if let Some(delta) = choice.get("delta") {
            let text = str_field(delta, "content");
            if !text.is_empty() {
                events.push(LlmEvent::TextDelta(text.to_owned()));
            }
            for call in array(delta.get("tool_calls")) {
                self.on_tool_call(call, events);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
            for call in std::mem::take(&mut self.calls).into_values() {
                events.push(LlmEvent::ToolDone {
                    name: FUNCTION_CALL.to_owned(),
                    details: json!({
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": call.arguments,
                    }),
                });
                events.push(LlmEvent::FunctionCall {
                    call_id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                });
            }
        }
    }

    fn on_tool_call(&mut self, call: &Value, events: &mut Vec<LlmEvent>) {
        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let function = call.get("function");
        let pending = self.calls.entry(index).or_default();
        if pending.id.is_empty()
            && let Some(id) = call.get("id").and_then(Value::as_str)
        {
            id.clone_into(&mut pending.id);
            pending.name = function
                .map(|f| str_field(f, "name").to_owned())
                .unwrap_or_default();
            events.push(LlmEvent::ToolStart {
                name: FUNCTION_CALL.to_owned(),
                details: json!({"index": index, "call_id": pending.id, "name": pending.name}),
            });
        }
        if let Some(args) = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
        {
            pending.arguments.push_str(args);
        }
    }

    fn completed(&mut self) -> LlmEvent {
        LlmEvent::Completed {
            usage: self.usage.take(),
            response_id: self.response_id.clone(),
            incomplete_reason: incomplete_reason(self.finish_reason.as_deref()),
        }
    }
}

#[cfg(test)]
#[path = "chat_completions_tests.rs"]
mod chat_completions_tests;
