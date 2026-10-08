//! Provider request bodies per adapter kind (DESIGN §3.5, "Provider Request Metadata",
//! "Multimodal Input Format", ADR-0005).

use serde_json::{Map, Value, json};

use super::super::{ContentPart, InputMessage, InputRole, LlmRequest, ToolExchange, ToolSpec};
use crate::config::ProviderKind;

/// Top-level keys the request controls; `extra_body` entries with these keys are ignored.
pub(crate) const CONTROLLED_KEYS: &[&str] = &[
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

/// `anthropic-version` header value of the Anthropic Messages adapter.
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Builds the JSON body for `kind`.
pub(crate) fn build_body(kind: ProviderKind, req: &LlmRequest) -> Value {
    match kind {
        ProviderKind::OpenaiResponses => responses_body(req, false),
        ProviderKind::VllmResponses => responses_body(req, true),
        ProviderKind::OpenaiChatCompletions => chat_completions_body(req),
        ProviderKind::AnthropicMessages => anthropic_body(req),
    }
}

/// Request path: `api_path` with `{model}` replaced by the provider model name.
pub(crate) fn chat_path(api_path: &str, model: &str) -> String {
    api_path.replace("{model}", model)
}

fn responses_input(msg: &InputMessage) -> Value {
    let content: Vec<Value> = match msg.role {
        InputRole::Assistant => msg
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(json!({ "type": "output_text", "text": t })),
                ContentPart::Image { .. } => None,
            })
            .collect(),
        InputRole::User | InputRole::System => msg
            .content
            .iter()
            .map(|p| match p {
                ContentPart::Text(t) => json!({ "type": "input_text", "text": t }),
                ContentPart::Image { file_id } => {
                    json!({ "type": "input_image", "file_id": file_id })
                }
            })
            .collect(),
    };
    json!({ "role": msg.role.as_str(), "content": content })
}

fn insert_api_params(body: &mut Map<String, Value>, req: &LlmRequest, reasoning_object: bool) {
    let p = &req.api_params;
    for (key, val) in [
        ("temperature", p.temperature),
        ("top_p", p.top_p),
        ("frequency_penalty", p.frequency_penalty),
        ("presence_penalty", p.presence_penalty),
    ] {
        if let Some(v) = val {
            body.insert(key.to_owned(), json!(v));
        }
    }
    if let Some(effort) = p
        .reasoning_effort
        .as_deref()
        .filter(|e| !e.trim().is_empty())
    {
        if reasoning_object {
            body.insert("reasoning".to_owned(), json!({ "effort": effort }));
        } else {
            body.insert("reasoning_effort".to_owned(), json!(effort));
        }
    }
}

fn merge_extra_body(body: &mut Map<String, Value>, req: &LlmRequest) {
    let Some(extra) = &req.api_params.extra_body else {
        return;
    };
    for (key, val) in extra {
        if CONTROLLED_KEYS.contains(&key.as_str()) {
            tracing::warn!(key = %key, "extra_body key controlled by the request is ignored");
            continue;
        }
        body.insert(key.clone(), val.clone());
    }
}

fn responses_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| match t {
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
            } => json!({
                "type": "web_search",
                "search_context_size": search_context_size,
            }),
            ToolSpec::CodeInterpreter { file_ids } => json!({
                "type": "code_interpreter",
                "container": { "type": "auto", "file_ids": file_ids },
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
        })
        .collect()
}

/// Responses API input items of the earlier agentic iterations.
fn responses_exchanges(exchanges: &[ToolExchange]) -> impl Iterator<Item = Value> + '_ {
    exchanges.iter().flat_map(|x| {
        [
            json!({
                "type": "function_call",
                "call_id": x.call_id,
                "name": x.name,
                "arguments": x.arguments,
            }),
            json!({
                "type": "function_call_output",
                "call_id": x.call_id,
                "output": x.output,
            }),
        ]
    })
}

/// Parses function-call arguments into a JSON object (`{}` when invalid).
fn arguments_object(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// OpenAI Responses (`vllm = false`) / vLLM Responses (`vllm = true`: no tools, no
/// `metadata`).
pub(crate) fn responses_body(req: &LlmRequest, vllm: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    body.insert(
        "input".to_owned(),
        Value::Array(
            req.input
                .iter()
                .map(responses_input)
                .chain(responses_exchanges(&req.tool_exchanges))
                .collect(),
        ),
    );
    if !req.instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(req.instructions));
    }
    body.insert("stream".to_owned(), json!(req.stream));
    body.insert("max_output_tokens".to_owned(), json!(req.max_output_tokens));
    body.insert("user".to_owned(), json!(req.user));
    if !vllm {
        body.insert("metadata".to_owned(), json!(req.metadata));
    }
    body.insert("store".to_owned(), json!(false));
    if !vllm && !req.tools.is_empty() {
        body.insert(
            "tools".to_owned(),
            Value::Array(responses_tools(&req.tools)),
        );
        if req
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
        {
            body.insert(
                "include".to_owned(),
                json!(["code_interpreter_call.outputs"]),
            );
        }
        if let Some(n) = req.max_tool_calls {
            body.insert("max_tool_calls".to_owned(), json!(n));
        }
    }
    insert_api_params(&mut body, req, true);
    merge_extra_body(&mut body, req);
    Value::Object(body)
}

fn text_of(msg: &InputMessage) -> String {
    msg.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// OpenAI Chat Completions: built-in tools are dropped (function tools are kept); images
/// (provider file ids) are not expressible and are dropped.
pub(crate) fn chat_completions_body(req: &LlmRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": req.instructions }));
    }
    for m in &req.input {
        messages.push(json!({ "role": m.role.as_str(), "content": text_of(m) }));
    }
    for x in &req.tool_exchanges {
        messages.push(json!({
            "role": "assistant",
            "content": Value::Null,
            "tool_calls": [{
                "id": x.call_id,
                "type": "function",
                "function": { "name": x.name, "arguments": x.arguments },
            }],
        }));
        messages.push(json!({
            "role": "tool",
            "tool_call_id": x.call_id,
            "content": x.output,
        }));
    }
    let function_tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                },
            })),
            _ => None,
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("stream".to_owned(), json!(req.stream));
    if req.stream {
        body.insert(
            "stream_options".to_owned(),
            json!({ "include_usage": true }),
        );
    }
    body.insert(
        "max_completion_tokens".to_owned(),
        json!(req.max_output_tokens),
    );
    body.insert("user".to_owned(), json!(req.user));
    if !function_tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(function_tools));
    }
    if !req.api_params.stop.is_empty() {
        body.insert("stop".to_owned(), json!(req.api_params.stop));
    }
    insert_api_params(&mut body, req, false);
    merge_extra_body(&mut body, req);
    Value::Object(body)
}

/// Anthropic Messages: `system` from instructions (+ system-role input), `file_search`
/// dropped, `web_search` / `code_interpreter` mapped to server tools, user identity in
/// `metadata.user_id`. `extra_body` is not sent.
pub(crate) fn anthropic_body(req: &LlmRequest) -> Value {
    let mut system = req.instructions.clone();
    let mut messages = Vec::new();
    for m in &req.input {
        if m.role == InputRole::System {
            let t = text_of(m);
            if !t.is_empty() {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&t);
            }
            continue;
        }
        let content: Vec<Value> = m
            .content
            .iter()
            .map(|p| match p {
                ContentPart::Text(t) => json!({ "type": "text", "text": t }),
                ContentPart::Image { file_id } => json!({
                    "type": "image",
                    "source": { "type": "file", "file_id": file_id },
                }),
            })
            .collect();
        messages.push(json!({ "role": m.role.as_str(), "content": content }));
    }
    for x in &req.tool_exchanges {
        messages.push(json!({
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": x.call_id,
                "name": x.name,
                "input": arguments_object(&x.arguments),
            }],
        }));
        messages.push(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": x.call_id,
                "content": x.output,
            }],
        }));
    }
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    if !system.is_empty() {
        body.insert("system".to_owned(), json!(system));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("max_tokens".to_owned(), json!(req.max_output_tokens));
    body.insert("stream".to_owned(), json!(req.stream));
    body.insert("metadata".to_owned(), json!({ "user_id": req.user }));
    let p = &req.api_params;
    if let Some(t) = p.temperature {
        body.insert("temperature".to_owned(), json!(t));
    }
    if let Some(t) = p.top_p {
        body.insert("top_p".to_owned(), json!(t));
    }
    if !p.stop.is_empty() {
        body.insert("stop_sequences".to_owned(), json!(p.stop));
    }
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::FileSearch { .. } => None,
            ToolSpec::WebSearch { .. } => Some(json!({
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": req.max_tool_calls.unwrap_or(2),
            })),
            ToolSpec::CodeInterpreter { .. } => Some(json!({
                "type": "code_execution_20250825",
                "name": "code_execution",
            })),
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => Some(json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            })),
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    Value::Object(body)
}
