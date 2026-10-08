//! Anthropic Messages API adapter (`kind: anthropic_messages`, S§9.2,
//! ADR-0005).
//!
//! Request: `system`, `messages`, `max_tokens`, `metadata.user_id` (the
//! provider `user` value), `stream`; `temperature`, `top_p` and `stop`
//! (`stop_sequences`) when set; no `extra_body`, no `max_tool_calls`.
//! Tools: `file_search` is dropped, `web_search` becomes the server web
//! search tool, `code_interpreter` the code execution tool, function tools
//! become client tools. Images are Anthropic Files API file ids (the
//! secondary copies). Every request carries `anthropic-version`; the code
//! execution tool adds its beta header.
//!
//! Stream: `message_start` (id, usage) → `content_block_*` → `message_delta`
//! (stop reason, usage) → `message_stop` (terminal). `stop_reason =
//! max_tokens` is an incomplete response with reason `max_tokens`. Code
//! execution calls are tool events named `code_interpreter` (start at the
//! block start, done at its stop); a client tool use is a `start` event
//! named `search_knowledge`, `load_files` or `unknown_tool` and an
//! [`LlmEvent::FunctionCall`] when its input is complete. No citations.
//!
//! Usage: Anthropic's `input_tokens` excludes cached input, so the internal
//! `input_tokens` is `input_tokens + cache_creation_input_tokens +
//! cache_read_input_tokens` (the cache counts are subsets of it, D§5.5.9).

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};
use tracing::{debug, warn};

use super::openai_responses::{error_details, failure, str_field, unparseable_event};
use super::{ParseState, PendingCall, ProviderAdapter};
use crate::infra::llm::sse_parser::SseEvent;
use crate::infra::llm::types::{
    ContentPart, InputMessage, LlmCompletion, LlmEvent, LlmRequest, LlmTerminal, ProviderFailure,
    ToolSpec,
};

/// `anthropic-version` header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server web search tool type.
pub const WEB_SEARCH_TOOL: &str = "web_search_20250305";
/// Code execution tool type.
pub const CODE_EXECUTION_TOOL: &str = "code_execution_20250825";
/// Beta header value the code execution tool needs.
pub const CODE_EXECUTION_BETA: &str = "code-execution-2025-08-25";

/// Server tool names of code execution calls (the current tool's bash and
/// text editor calls, and the legacy tool's name).
const CODE_EXECUTION_CALLS: &[&str] = &[
    "bash_code_execution",
    "text_editor_code_execution",
    "code_execution",
];

/// Anthropic Messages adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnthropicMessagesAdapter;

impl ProviderAdapter for AnthropicMessagesAdapter {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(req.model));
        if !req.instructions.is_empty() {
            body.insert("system".into(), json!(req.instructions));
        }
        body.insert(
            "messages".into(),
            Value::Array(req.input.iter().map(message).collect()),
        );
        body.insert("max_tokens".into(), json!(req.max_output_tokens));
        body.insert("stream".into(), json!(req.stream));
        body.insert("metadata".into(), json!({"user_id": req.user}));
        let tools: Vec<Value> = req.tools.iter().filter_map(tool).collect();
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        let p = &req.api_params;
        for (key, value) in [("temperature", p.temperature), ("top_p", p.top_p)] {
            if let Some(v) = value {
                body.insert(key.into(), json!(v));
            }
        }
        if !p.stop.is_empty() {
            body.insert("stop_sequences".into(), json!(p.stop));
        }
        Value::Object(body)
    }

    fn extra_headers(&self, req: &LlmRequest) -> Vec<(&'static str, String)> {
        let mut headers = vec![("anthropic-version", ANTHROPIC_VERSION.to_owned())];
        if req
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
        {
            headers.push(("anthropic-beta", CODE_EXECUTION_BETA.to_owned()));
        }
        headers
    }

    fn parse_event(&self, event: &SseEvent, state: &mut ParseState) -> Vec<LlmEvent> {
        let Some(name) = event.name() else {
            debug!("provider SSE event without a name ignored");
            return Vec::new();
        };
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(e) => return unparseable_event(&name, &event.data, &e),
        };
        match name.as_str() {
            "message_start" => {
                let message = &data["message"];
                state.response_id = message.get("id").and_then(Value::as_str).map(str::to_owned);
                state.usage = message.get("usage").filter(|u| u.is_object()).cloned();
                Vec::new()
            }
            "content_block_start" => block_start(&data, state),
            "content_block_delta" => block_delta(&data, state),
            "content_block_stop" => block_stop(&data, state),
            "message_delta" => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    state.stop_reason = Some(reason.to_owned());
                }
                if let Some(Value::Object(delta)) = data.get("usage") {
                    let usage = state.usage.get_or_insert_with(|| json!({}));
                    if let Some(obj) = usage.as_object_mut() {
                        for (k, v) in delta {
                            obj.insert(k.clone(), v.clone());
                        }
                    }
                }
                Vec::new()
            }
            "message_stop" => vec![terminal(state)],
            "error" => {
                let (code, message) = error_details(&data);
                warn!(provider_code = ?code, provider_message = %message, "provider reported a failure");
                vec![LlmEvent::Failed(failure(&message, None))]
            }
            _ => Vec::new(),
        }
    }

    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderFailure> {
        if body.get("type").and_then(Value::as_str) == Some("error")
            || !body.get("content").is_some_and(Value::is_array)
        {
            let (code, message) = error_details(body);
            warn!(provider_code = ?code, provider_message = %message, "provider non-streaming call failed");
            return Err(failure(&message, None));
        }
        let text = body["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .map(|b| str_field(b, "text"))
            .collect::<String>();
        Ok(LlmCompletion {
            text,
            usage: usage(body.get("usage")),
            response_id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

/// One message: string content when it is plain text, content blocks
/// otherwise (text, file-id images, `tool_use`, `tool_result`).
fn message(m: &InputMessage) -> Value {
    let plain = m.content.iter().all(|p| matches!(p, ContentPart::Text(_)));
    let content = if plain {
        json!(super::openai_responses::joined_text(m))
    } else {
        Value::Array(m.content.iter().map(content_block).collect())
    };
    json!({"role": m.role.as_str(), "content": content})
}

fn content_block(p: &ContentPart) -> Value {
    match p {
        ContentPart::Text(text) => json!({"type": "text", "text": text}),
        ContentPart::Image { file_id } => {
            json!({"type": "image", "source": {"type": "file", "file_id": file_id}})
        }
        ContentPart::FunctionCall {
            call_id,
            name,
            arguments,
        } => json!({
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": serde_json::from_str::<Value>(arguments)
                .ok()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({})),
        }),
        ContentPart::FunctionOutput { call_id, output } => json!({
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": output,
        }),
    }
}

fn tool(t: &ToolSpec) -> Option<Value> {
    match t {
        ToolSpec::FileSearch { .. } => None,
        ToolSpec::WebSearch { .. } => Some(json!({"type": WEB_SEARCH_TOOL, "name": "web_search"})),
        ToolSpec::CodeInterpreter { .. } => {
            Some(json!({"type": CODE_EXECUTION_TOOL, "name": "code_execution"}))
        }
        ToolSpec::Function {
            name,
            description,
            parameters,
        } => Some(json!({
            "name": name,
            "description": description,
            "input_schema": parameters,
        })),
    }
}

fn block_index(data: &Value) -> u64 {
    data.get("index").and_then(Value::as_u64).unwrap_or(0)
}

fn tool_event(start: bool, name: &str) -> LlmEvent {
    if start {
        LlmEvent::ToolStart {
            name: name.into(),
            details: json!({}),
        }
    } else {
        LlmEvent::ToolDone {
            name: name.into(),
            details: json!({}),
        }
    }
}

/// Client tool name → tool event name.
fn tool_use_event_name(name: &str) -> &'static str {
    match name {
        "search_knowledge" => "search_knowledge",
        "load_files" => "load_files",
        _ => "unknown_tool",
    }
}

fn block_start(data: &Value, state: &mut ParseState) -> Vec<LlmEvent> {
    let index = block_index(data);
    let block = &data["content_block"];
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            let text = str_field(block, "text");
            if text.is_empty() {
                Vec::new()
            } else {
                vec![LlmEvent::TextDelta(text.to_owned())]
            }
        }
        Some("server_tool_use") => {
            let name = str_field(block, "name");
            if name == "web_search" {
                vec![tool_event(true, "web_search")]
            } else if CODE_EXECUTION_CALLS.contains(&name) {
                state.code_blocks.insert(index);
                vec![tool_event(true, "code_interpreter")]
            } else {
                Vec::new()
            }
        }
        Some("web_search_tool_result") => vec![tool_event(false, "web_search")],
        Some("tool_use") => {
            let name = str_field(block, "name");
            state.calls.insert(
                index,
                PendingCall {
                    call_id: str_field(block, "id").to_owned(),
                    name: name.to_owned(),
                    arguments: String::new(),
                },
            );
            vec![tool_event(true, tool_use_event_name(name))]
        }
        _ => Vec::new(),
    }
}

fn block_delta(data: &Value, state: &mut ParseState) -> Vec<LlmEvent> {
    let delta = &data["delta"];
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => {
            let text = str_field(delta, "text");
            if text.is_empty() {
                Vec::new()
            } else {
                vec![LlmEvent::TextDelta(text.to_owned())]
            }
        }
        Some("input_json_delta") => {
            if let Some(call) = state.calls.get_mut(&block_index(data)) {
                call.arguments.push_str(str_field(delta, "partial_json"));
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn block_stop(data: &Value, state: &mut ParseState) -> Vec<LlmEvent> {
    let index = block_index(data);
    if state.code_blocks.remove(&index) {
        return vec![tool_event(false, "code_interpreter")];
    }
    match state.calls.remove(&index) {
        Some(call) => vec![LlmEvent::FunctionCall {
            call_id: call.call_id,
            name: call.name,
            arguments: if call.arguments.trim().is_empty() {
                "{}".to_owned()
            } else {
                call.arguments
            },
        }],
        None => Vec::new(),
    }
}

/// `message_stop`: incomplete (`max_tokens`) or completed.
fn terminal(state: &ParseState) -> LlmEvent {
    let terminal = LlmTerminal {
        usage: usage(state.usage.as_ref()),
        response_id: state.response_id.clone(),
    };
    if state.stop_reason.as_deref() == Some("max_tokens") {
        LlmEvent::Incomplete {
            terminal,
            reason: "max_tokens".into(),
        }
    } else {
        LlmEvent::Completed(terminal)
    }
}

/// Anthropic usage → internal usage (cache counts included in the input).
fn usage(v: Option<&Value>) -> Option<UsageTokens> {
    let u = v.filter(|u| u.is_object())?;
    let int = |key: &str| u.get(key).and_then(Value::as_i64).unwrap_or(0);
    let (cache_write, cache_read) = (
        int("cache_creation_input_tokens"),
        int("cache_read_input_tokens"),
    );
    Some(UsageTokens {
        input_tokens: int("input_tokens") + cache_write + cache_read,
        output_tokens: int("output_tokens"),
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: cache_write,
        reasoning_tokens: 0,
    })
}

#[cfg(test)]
#[path = "anthropic_messages_tests.rs"]
mod tests;
