//! Anthropic Messages adapter: `file_search` is dropped, `web_search` and
//! `code_interpreter` map to Anthropic server tools.

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::{ContentPart, LlmRequest, ProviderEvent, ToolSpec, TranslateState, codes};

#[must_use]
pub fn build_body(req: &LlmRequest) -> Value {
    let messages: Vec<Value> = req
        .input
        .iter()
        .map(|m| {
            let content: Vec<Value> = m
                .parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text(t) => json!({ "type": "text", "text": t }),
                    ContentPart::Image(id) => json!({ "type": "image", "source": { "type": "file", "file_id": id } }),
                })
                .collect();
            json!({ "role": m.role, "content": content })
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(req.provider_model_id));
    body.insert("messages".into(), Value::Array(messages));
    if !req.instructions.is_empty() {
        body.insert("system".into(), json!(req.instructions));
    }
    body.insert("max_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(req.stream));
    body.insert("metadata".into(), json!({ "user_id": req.user }));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::FileSearch { .. } => None,
            ToolSpec::WebSearch { .. } => Some(json!({ "type": "web_search_20250305", "name": "web_search" })),
            ToolSpec::CodeInterpreter { .. } => Some(json!({ "type": "code_execution_20250522", "name": "code_execution" })),
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(t) = req.api_params.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(t) = req.api_params.top_p {
        body.insert("top_p".into(), json!(t));
    }
    if !req.api_params.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(req.api_params.stop));
    }
    Value::Object(body)
}

fn usage_mut(state: &mut TranslateState) -> &mut UsageTokens {
    state.anthropic_usage.get_or_insert_with(UsageTokens::default)
}

pub fn translate(state: &mut TranslateState, event: Option<&str>, data: &str) -> Vec<ProviderEvent> {
    let Ok(v) = serde_json::from_str::<Value>(data.trim()) else {
        return Vec::new();
    };
    let name = event
        .filter(|e| !e.is_empty() && *e != "message")
        .map(str::to_owned)
        .or_else(|| v.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    match name.as_str() {
        "message_start" => {
            if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                let usage = usage_mut(state);
                usage.input_tokens = u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                usage.cache_read_input_tokens = u.get("cache_read_input_tokens").and_then(Value::as_i64).unwrap_or(0);
                usage.cache_write_input_tokens = u.get("cache_creation_input_tokens").and_then(Value::as_i64).unwrap_or(0);
            }
            Vec::new()
        }
        "content_block_start" => {
            let block = v.get("content_block").unwrap_or(&Value::Null);
            let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
            match block.get("type").and_then(Value::as_str) {
                Some("server_tool_use") => {
                    let raw = block.get("name").and_then(Value::as_str).unwrap_or_default();
                    let mapped = match raw {
                        "web_search" => "web_search",
                        "code_execution" | "bash_code_execution" | "text_editor_code_execution" => "code_interpreter",
                        _ => "unknown_tool",
                    };
                    state.open_blocks.push((idx, mapped.to_owned()));
                    vec![ProviderEvent::ToolStart { name: mapped.into(), details: json!({}) }]
                }
                Some("tool_use") => {
                    let raw = block.get("name").and_then(Value::as_str).unwrap_or_default();
                    let mapped = if matches!(raw, "search_knowledge" | "load_files") { raw } else { "unknown_tool" };
                    vec![ProviderEvent::ToolStart { name: mapped.into(), details: json!({}) }]
                }
                _ => Vec::new(),
            }
        }
        "content_block_delta" => {
            let delta = v.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => delta
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(|t| vec![ProviderEvent::TextDelta(t.to_owned())])
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
        "content_block_stop" => {
            let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
            if let Some(pos) = state.open_blocks.iter().position(|(i, _)| *i == idx) {
                let (_, name) = state.open_blocks.remove(pos);
                if name != "unknown_tool" {
                    return vec![ProviderEvent::ToolDone { name, details: json!({}) }];
                }
            }
            Vec::new()
        }
        "message_delta" => {
            if let Some(o) = v.get("usage").and_then(|u| u.get("output_tokens")).and_then(Value::as_i64) {
                usage_mut(state).output_tokens = o;
            }
            if let Some(r) = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                state.anthropic_stop = Some(r.to_owned());
            }
            Vec::new()
        }
        "message_stop" => {
            let incomplete_reason = (state.anthropic_stop.as_deref() == Some("max_tokens")).then(|| "max_tokens".to_owned());
            vec![ProviderEvent::Completed { usage: state.anthropic_usage, response_id: None, incomplete_reason }]
        }
        "error" => {
            let message = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Provider returned an error");
            vec![ProviderEvent::Failed { code: codes::PROVIDER_ERROR, message: message.to_owned(), usage: None }]
        }
        _ => Vec::new(),
    }
}

#[must_use]
pub fn parse_completion(body: &Value) -> (String, Option<UsageTokens>) {
    let text: String = body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    let usage = body.get("usage").map(|u| UsageTokens {
        input_tokens: u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0),
        output_tokens: u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0),
        cache_read_input_tokens: u.get("cache_read_input_tokens").and_then(Value::as_i64).unwrap_or(0),
        cache_write_input_tokens: u.get("cache_creation_input_tokens").and_then(Value::as_i64).unwrap_or(0),
        reasoning_tokens: 0,
    });
    (text, usage)
}
