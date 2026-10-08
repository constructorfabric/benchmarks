//! Chat Completions API adapter (`kind: openai_chat_completions`, S§9.2,
//! ADR-0005).
//!
//! The request carries `messages` (system first), `max_completion_tokens`,
//! `user` and only the function tools: `file_search`, `web_search` and
//! `code_interpreter` are dropped, and so are image inputs (Chat Completions
//! cannot reference uploaded image files). The stream is a sequence of
//! unnamed `chat.completion.chunk` events ended by `data: [DONE]`, which
//! produces the terminal event; `finish_reason = length` is an incomplete
//! response with reason `max_tokens`. Tool calls are reported as tool events
//! named `function_call` and as [`LlmEvent::FunctionCall`].

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};
use tracing::{debug, warn};

use super::openai_responses::{CONTROLLED_KEYS, error_details, failure, joined_text, str_field};
use super::{ParseState, PendingCall, ProviderAdapter};
use crate::infra::llm::sse_parser::SseEvent;
use crate::infra::llm::types::{
    ContentPart, InputMessage, LlmCompletion, LlmEvent, LlmRequest, LlmTerminal, ProviderFailure,
    ToolSpec,
};

/// Tool event name of function calls.
const FUNCTION_CALL_EVENT: &str = "function_call";
/// Stream end marker.
const DONE: &str = "[DONE]";

/// Chat Completions adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAiChatCompletionsAdapter;

impl ProviderAdapter for OpenAiChatCompletionsAdapter {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut messages = vec![json!({"role": "system", "content": req.instructions})];
        messages.extend(req.input.iter().flat_map(message));
        let mut body = Map::new();
        body.insert("model".into(), json!(req.model));
        body.insert("messages".into(), Value::Array(messages));
        body.insert("stream".into(), json!(req.stream));
        if req.stream {
            body.insert("stream_options".into(), json!({"include_usage": true}));
        }
        body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
        body.insert("user".into(), json!(req.user));
        let tools: Vec<Value> = req.tools.iter().filter_map(function_tool).collect();
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }

        let p = &req.api_params;
        for (key, value) in [
            ("temperature", p.temperature),
            ("top_p", p.top_p),
            ("frequency_penalty", p.frequency_penalty),
            ("presence_penalty", p.presence_penalty),
        ] {
            if let Some(v) = value {
                body.insert(key.into(), json!(v));
            }
        }
        if !p.stop.is_empty() {
            body.insert("stop".into(), json!(p.stop));
        }
        if let Some(effort) = &p.reasoning_effort {
            body.insert("reasoning_effort".into(), json!(effort));
        }
        if let Some(extra) = &p.extra_body {
            for (key, value) in extra {
                if CONTROLLED_KEYS.contains(&key.as_str()) {
                    warn!(key = %key, model = %req.model, "extra_body key controlled by the request is ignored");
                } else {
                    body.insert(key.clone(), value.clone());
                }
            }
        }
        Value::Object(body)
    }

    fn parse_event(&self, event: &SseEvent, state: &mut ParseState) -> Vec<LlmEvent> {
        if event.data.trim() == DONE {
            return vec![terminal(state)];
        }
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(e) => {
                debug!(error = %e, "Chat Completions chunk with invalid JSON ignored");
                return Vec::new();
            }
        };
        if data.get("error").is_some_and(Value::is_object) {
            let (code, message) = error_details(&data);
            warn!(provider_code = ?code, provider_message = %message, "provider reported a failure");
            return vec![LlmEvent::Failed(failure(&message, None))];
        }
        if state.response_id.is_none() {
            state.response_id = data.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        if let Some(u) = data.get("usage").filter(|u| u.is_object()) {
            state.usage = Some(u.clone());
        }
        data.pointer("/choices/0")
            .map(|choice| choice_events(choice, state))
            .unwrap_or_default()
    }

    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderFailure> {
        let Some(choice) = body.pointer("/choices/0") else {
            let (code, message) = error_details(body);
            warn!(provider_code = ?code, provider_message = %message, "provider non-streaming call failed");
            return Err(failure(&message, None));
        };
        Ok(LlmCompletion {
            text: str_field(&choice["message"], "content").to_owned(),
            usage: usage(body.get("usage")),
            response_id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

/// Text, tool-call and finish events of the first choice of a chunk.
fn choice_events(choice: &Value, state: &mut ParseState) -> Vec<LlmEvent> {
    let mut out = Vec::new();
    let delta = &choice["delta"];
    let text = str_field(delta, "content");
    if !text.is_empty() {
        out.push(LlmEvent::TextDelta(text.to_owned()));
    }
    for call in delta["tool_calls"].as_array().into_iter().flatten() {
        out.extend(tool_call_delta(call, state));
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        state.stop_reason = Some(reason.to_owned());
        out.extend(finish_calls(state));
    }
    out
}

/// Messages of one input message: a text message (images dropped), an
/// assistant message with `tool_calls`, or one `tool` message per output.
fn message(m: &InputMessage) -> Vec<Value> {
    let calls: Vec<Value> = m
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::FunctionCall {
                call_id,
                name,
                arguments,
            } => Some(json!({
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            })),
            _ => None,
        })
        .collect();
    let outputs: Vec<Value> = m
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::FunctionOutput { call_id, output } => Some(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output,
            })),
            _ => None,
        })
        .collect();
    if !calls.is_empty() {
        return vec![json!({"role": "assistant", "content": null, "tool_calls": calls})];
    }
    if !outputs.is_empty() {
        return outputs;
    }
    if m.content
        .iter()
        .any(|p| matches!(p, ContentPart::Image { .. }))
    {
        debug!("image inputs are not sent to a Chat Completions provider");
    }
    vec![json!({"role": m.role.as_str(), "content": joined_text(m)})]
}

/// Function tools only; built-in tools are dropped.
fn function_tool(t: &ToolSpec) -> Option<Value> {
    match t {
        ToolSpec::Function {
            name,
            description,
            parameters,
        } => Some(json!({"type": "function", "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }})),
        ToolSpec::FileSearch { .. }
        | ToolSpec::WebSearch { .. }
        | ToolSpec::CodeInterpreter { .. } => None,
    }
}

/// One `tool_calls` delta: the first fragment of an index starts the call.
fn tool_call_delta(call: &Value, state: &mut ParseState) -> Option<LlmEvent> {
    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
    let function = &call["function"];
    let fragment = str_field(function, "arguments");
    if let Some(pending) = state.calls.get_mut(&index) {
        pending.arguments.push_str(fragment);
        return None;
    }
    let pending = PendingCall {
        call_id: str_field(call, "id").to_owned(),
        name: str_field(function, "name").to_owned(),
        arguments: fragment.to_owned(),
    };
    let start = LlmEvent::ToolStart {
        name: FUNCTION_CALL_EVENT.into(),
        details: json!({"index": index, "call_id": pending.call_id, "name": pending.name}),
    };
    state.calls.insert(index, pending);
    Some(start)
}

/// The choice finished: every streamed call is complete.
fn finish_calls(state: &mut ParseState) -> Vec<LlmEvent> {
    std::mem::take(&mut state.calls)
        .into_values()
        .flat_map(|c| {
            [
                LlmEvent::ToolDone {
                    name: FUNCTION_CALL_EVENT.into(),
                    details: json!({"call_id": c.call_id, "name": c.name, "arguments": c.arguments}),
                },
                LlmEvent::FunctionCall {
                    call_id: c.call_id,
                    name: c.name,
                    arguments: c.arguments,
                },
            ]
        })
        .collect()
}

/// `[DONE]`: incomplete (`max_tokens`) after `finish_reason = length`,
/// completed otherwise.
fn terminal(state: &ParseState) -> LlmEvent {
    let terminal = LlmTerminal {
        usage: usage(state.usage.as_ref()),
        response_id: state.response_id.clone(),
    };
    if state.stop_reason.as_deref() == Some("length") {
        LlmEvent::Incomplete {
            terminal,
            reason: "max_tokens".into(),
        }
    } else {
        LlmEvent::Completed(terminal)
    }
}

/// `prompt_tokens` / `completion_tokens` (+ cached / reasoning details).
fn usage(v: Option<&Value>) -> Option<UsageTokens> {
    let u = v.filter(|u| u.is_object())?;
    let int = |ptr: &str| u.pointer(ptr).and_then(Value::as_i64).unwrap_or(0);
    Some(UsageTokens {
        input_tokens: int("/prompt_tokens"),
        output_tokens: int("/completion_tokens"),
        cache_read_input_tokens: int("/prompt_tokens_details/cached_tokens"),
        cache_write_input_tokens: 0,
        reasoning_tokens: int("/completion_tokens_details/reasoning_tokens"),
    })
}

#[cfg(test)]
#[path = "openai_chat_completions_tests.rs"]
mod tests;
