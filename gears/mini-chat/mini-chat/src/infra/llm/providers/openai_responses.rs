//! `OpenAI` / Azure `OpenAI` Responses API adapter (S§9.1, D "Provider Event
//! Translation").

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};
use tracing::{debug, warn};

use super::{ParseState, ProviderAdapter};
use crate::infra::llm::sanitize::sanitize_provider_message;
use crate::infra::llm::sse_parser::SseEvent;
use crate::infra::llm::types::{
    ContentPart, InputMessage, LlmCompletion, LlmEvent, LlmRequest, LlmTerminal, ProviderFailure,
    RawCitation, StreamErrorCode, ToolSpec,
};

/// `extra_body` keys the request controls (D "Model Catalog Configuration");
/// they are ignored with a warning.
pub const CONTROLLED_KEYS: &[&str] = &[
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

/// Cap of the `code_interpreter` tool output sent to clients (characters).
pub const CODE_OUTPUT_MAX_CHARS: usize = 8192;
const TRUNCATED_SUFFIX: &str = "...[truncated]";
const GENERIC_PROVIDER_ERROR: &str = "Provider returned an error";

/// Responses API adapter (`kind: openai_responses`).
#[derive(Debug, Default, Clone, Copy)]
pub struct OpenAiResponsesAdapter;

impl ProviderAdapter for OpenAiResponsesAdapter {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(req.model));
        body.insert("instructions".into(), json!(req.instructions));
        body.insert(
            "input".into(),
            Value::Array(req.input.iter().flat_map(input_items).collect()),
        );
        body.insert("stream".into(), json!(req.stream));
        body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
        body.insert("store".into(), json!(false));
        if !req.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(req.tools.iter().map(tool).collect()),
            );
        }
        body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
        if req
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
        {
            body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
        }
        body.insert("user".into(), json!(req.user));
        let m = &req.metadata;
        body.insert(
            "metadata".into(),
            json!({
                "tenant_id": m.tenant_id.to_string(),
                "user_id": m.user_id.to_string(),
                "chat_id": m.chat_id.to_string(),
                "request_type": m.request_type,
                "feature": m.feature,
            }),
        );

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
            body.insert("reasoning".into(), json!({ "effort": effort }));
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
        let Some(name) = event.name() else {
            debug!("provider SSE event without a name ignored");
            return Vec::new();
        };
        let data: Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(e) => return unparseable_event(&name, &event.data, &e),
        };
        if let Some(ev) = tool_event(&name, &data) {
            return vec![ev];
        }
        if let Some(ev) = terminal_event(&name, &data) {
            return vec![ev];
        }
        content_events(&name, &data, state)
    }

    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderFailure> {
        if body.get("status").and_then(Value::as_str) == Some("failed")
            || body.get("output").is_none()
        {
            let (code, message) = error_details(body);
            warn!(provider_code = ?code, provider_message = %message, "provider non-streaming call failed");
            return Err(failure(&message, usage(body.get("usage"))));
        }
        let text = body["output"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
            .flat_map(|item| item["content"].as_array().into_iter().flatten())
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("output_text"))
            .map(|c| str_field(c, "text"))
            .collect::<String>();
        Ok(LlmCompletion {
            text,
            usage: usage(body.get("usage")),
            response_id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        })
    }
}

/// Data that is not JSON: an `error` event's raw data becomes the message.
pub(super) fn unparseable_event(name: &str, data: &str, err: &serde_json::Error) -> Vec<LlmEvent> {
    if name == "error" {
        warn!(data = %data, "provider SSE error event (unparseable)");
        vec![LlmEvent::Failed(failure(data, None))]
    } else {
        debug!(event = %name, error = %err, "provider SSE event with invalid JSON ignored");
        Vec::new()
    }
}

/// Built-in tool start / done events.
fn tool_event(name: &str, data: &Value) -> Option<LlmEvent> {
    let done = |tool: &str, details: Value| LlmEvent::ToolDone {
        name: tool.into(),
        details,
    };
    Some(match name {
        "response.file_search_call.searching" => tool_start("file_search"),
        "response.file_search_call.completed" => {
            let files = data
                .get("results")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            done("file_search", json!({ "files_searched": files }))
        }
        "response.web_search_call.searching" => tool_start("web_search"),
        "response.web_search_call.completed" => done("web_search", json!({})),
        "response.code_interpreter_call.in_progress" => tool_start("code_interpreter"),
        _ => return None,
    })
}

/// `response.completed` / `incomplete` / `failed` and the `error` event.
fn terminal_event(name: &str, data: &Value) -> Option<LlmEvent> {
    Some(match name {
        "response.completed" => LlmEvent::Completed(terminal(&data["response"])),
        "response.incomplete" => {
            let response = &data["response"];
            let reason = response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            LlmEvent::Incomplete {
                terminal: terminal(response),
                reason,
            }
        }
        "response.failed" | "error" => {
            let (code, message) = error_details(data);
            warn!(event = %name, provider_code = ?code, provider_message = %message, "provider reported a failure");
            let usage = data.get("response").and_then(|r| usage(r.get("usage")));
            LlmEvent::Failed(failure(&message, usage))
        }
        _ => return None,
    })
}

/// Text deltas, annotations and finished output items. Reasoning events
/// (`response.reasoning_*`) have no client event: only the vLLM adapter
/// emits `reasoning` deltas, from `<think>` text (D "SSE delta").
fn content_events(name: &str, data: &Value, state: &mut ParseState) -> Vec<LlmEvent> {
    match name {
        "response.output_text.delta" => {
            let delta = str_field(data, "delta");
            state
                .text_parts
                .entry(part_key(data))
                .or_default()
                .push_str(delta);
            vec![LlmEvent::TextDelta(delta.to_owned())]
        }
        "response.output_text.annotation.added" => {
            let key = part_key(data);
            state.annotated_items.insert(key.0);
            let text = state.text_parts.get(&key).map_or("", String::as_str);
            data.get("annotation")
                .and_then(|a| citation(a, text))
                .map(LlmEvent::Citation)
                .into_iter()
                .collect()
        }
        "response.output_item.done" => output_item_done(data, state),
        _ => Vec::new(),
    }
}

/// Input items of one message: the message itself, or one
/// `function_call` / `function_call_output` item per replayed part.
fn input_items(m: &InputMessage) -> Vec<Value> {
    let function_items: Vec<Value> = m
        .content
        .iter()
        .filter_map(|p| match p {
            ContentPart::FunctionCall {
                call_id,
                name,
                arguments,
            } => Some(json!({
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
            })),
            ContentPart::FunctionOutput { call_id, output } => Some(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            })),
            ContentPart::Text(_) | ContentPart::Image { .. } => None,
        })
        .collect();
    if function_items.is_empty() {
        vec![input_message(m)]
    } else {
        function_items
    }
}

fn input_message(m: &InputMessage) -> Value {
    let has_image = m
        .content
        .iter()
        .any(|p| matches!(p, ContentPart::Image { .. }));
    let content = if has_image {
        Value::Array(
            m.content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text(text) => Some(json!({"type": "input_text", "text": text})),
                    ContentPart::Image { file_id } => {
                        Some(json!({"type": "input_image", "file_id": file_id}))
                    }
                    ContentPart::FunctionCall { .. } | ContentPart::FunctionOutput { .. } => None,
                })
                .collect(),
        )
    } else {
        json!(joined_text(m))
    };
    json!({"role": m.role.as_str(), "content": content})
}

/// The text parts of `m` joined with `\n`.
pub(super) fn joined_text(m: &InputMessage) -> String {
    m.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool(t: &ToolSpec) -> Value {
    match t {
        ToolSpec::FileSearch {
            vector_store_ids,
            max_num_results,
        } => json!({
            "type": "file_search",
            "vector_store_ids": vector_store_ids,
            "max_num_results": max_num_results,
        }),
        ToolSpec::WebSearch {
            search_context_size,
        } => json!({"type": "web_search", "search_context_size": search_context_size}),
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": {"type": "auto", "file_ids": file_ids},
        }),
        ToolSpec::Function {
            name,
            description,
            parameters,
        } => json!({
            "type": "function",
            "name": name,
            "description": description,
            "parameters": parameters,
        }),
    }
}

pub(super) fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn part_key(data: &Value) -> (u64, u64) {
    let idx = |k: &str| data.get(k).and_then(Value::as_u64).unwrap_or(0);
    (idx("output_index"), idx("content_index"))
}

fn tool_start(name: &str) -> LlmEvent {
    LlmEvent::ToolStart {
        name: name.into(),
        details: json!({}),
    }
}

fn output_item_done(data: &Value, state: &ParseState) -> Vec<LlmEvent> {
    let item = &data["item"];
    match item.get("type").and_then(Value::as_str) {
        Some("code_interpreter_call") => {
            let logs: Vec<&str> = item["outputs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
                .map(|o| str_field(o, "logs"))
                .collect();
            vec![LlmEvent::ToolDone {
                name: "code_interpreter".into(),
                details: json!({ "output": cap_output(&logs.join("\n")) }),
            }]
        }
        Some("function_call") => vec![LlmEvent::FunctionCall {
            call_id: str_field(item, "call_id").to_owned(),
            name: str_field(item, "name").to_owned(),
            arguments: str_field(item, "arguments").to_owned(),
        }],
        Some("message") => {
            let output_index = data
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if state.annotated_items.contains(&output_index) {
                return Vec::new();
            }
            item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|part| {
                    let text = str_field(part, "text");
                    part["annotations"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(move |a| citation(a, text))
                })
                .map(LlmEvent::Citation)
                .collect()
        }
        _ => Vec::new(),
    }
}

fn cap_output(s: &str) -> String {
    match s.char_indices().nth(CODE_OUTPUT_MAX_CHARS) {
        Some((byte_idx, _)) => format!("{}{TRUNCATED_SUFFIX}", &s[..byte_idx]),
        None => s.to_owned(),
    }
}

/// Map one provider annotation; `text` is the output text part carrying it.
fn citation(a: &Value, text: &str) -> Option<RawCitation> {
    match a.get("type").and_then(Value::as_str)? {
        "url_citation" => {
            let index = |k: &str| {
                a.get(k)
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
            };
            let (start, end) = (index("start_index"), index("end_index"));
            let snippet = match a.get("text").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => t.to_owned(),
                _ => char_range(text, start, end),
            };
            Some(RawCitation::Url {
                url: str_field(a, "url").to_owned(),
                title: str_field(a, "title").to_owned(),
                start,
                end,
                snippet,
            })
        }
        "file_citation" => Some(RawCitation::File {
            provider_file_id: str_field(a, "file_id").to_owned(),
            filename: str_field(a, "filename").to_owned(),
        }),
        _ => None,
    }
}

/// Characters `[start, end)` of `text`; empty when the range is missing or
/// outside the text.
fn char_range(text: &str, start: Option<usize>, end: Option<usize>) -> String {
    let (Some(start), Some(end)) = (start, end) else {
        return String::new();
    };
    if start >= end || end > text.chars().count() {
        return String::new();
    }
    text.chars().skip(start).take(end - start).collect()
}

pub(super) fn usage(v: Option<&Value>) -> Option<UsageTokens> {
    let u = v.filter(|u| u.is_object())?;
    let int = |ptr: &str| u.pointer(ptr).and_then(Value::as_i64).unwrap_or(0);
    Some(UsageTokens {
        input_tokens: int("/input_tokens"),
        output_tokens: int("/output_tokens"),
        cache_read_input_tokens: int("/input_tokens_details/cached_tokens"),
        cache_write_input_tokens: 0,
        reasoning_tokens: int("/output_tokens_details/reasoning_tokens"),
    })
}

fn terminal(response: &Value) -> LlmTerminal {
    LlmTerminal {
        usage: usage(response.get("usage")),
        response_id: response
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

/// Provider `(code, message)` of a failure: `response.error`, then a
/// top-level `error` object, then flat `{code, message}`.
pub(super) fn error_details(data: &Value) -> (Option<String>, String) {
    let obj = [
        data.pointer("/response/error"),
        data.get("error"),
        Some(data),
    ]
    .into_iter()
    .flatten()
    .find(|e| e.get("message").is_some_and(Value::is_string));
    obj.map_or((None, String::new()), |e| {
        (
            e.get("code").and_then(Value::as_str).map(str::to_owned),
            str_field(e, "message").to_owned(),
        )
    })
}

/// Sanitized `provider_error` failure.
pub(super) fn failure(provider_message: &str, usage: Option<UsageTokens>) -> ProviderFailure {
    let message = if provider_message.trim().is_empty() {
        GENERIC_PROVIDER_ERROR.to_owned()
    } else {
        sanitize_provider_message(provider_message)
    };
    ProviderFailure {
        code: StreamErrorCode::ProviderError,
        message,
        usage,
    }
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod tests;
