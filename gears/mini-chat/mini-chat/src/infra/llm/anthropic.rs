//! Anthropic Messages adapter (`anthropic_messages`): drops `file_search`,
//! maps `web_search` / `code_interpreter` to Anthropic server tools.

use serde_json::{Map, Value, json};

use super::{Adapter, ChatItem, Completion, ItemRole, LlmEvent, LlmFailure, LlmRequest, ParseState, Usage, error_message};

pub struct AnthropicMessages;

fn content_of(item: &ChatItem) -> Value {
    if item.images.is_empty() {
        return json!(item.text);
    }
    let mut parts = Vec::new();
    for f in &item.images {
        parts.push(json!({"type": "image", "source": {"type": "file", "file_id": f}}));
    }
    parts.push(json!({"type": "text", "text": item.text}));
    Value::Array(parts)
}

fn usage_of(v: &Value) -> Usage {
    let get = |k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    Usage {
        input_tokens: get("input_tokens") + get("cache_read_input_tokens") + get("cache_creation_input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_input_tokens: get("cache_read_input_tokens"),
        cache_write_input_tokens: get("cache_creation_input_tokens"),
        reasoning_tokens: 0,
    }
}

impl Adapter for AnthropicMessages {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(req.model));
        body.insert("stream".into(), json!(req.stream));
        body.insert("max_tokens".into(), json!(req.max_output_tokens));
        body.insert("system".into(), json!(req.instructions));
        let mut msgs: Vec<Value> = req
            .items
            .iter()
            .map(|it| json!({"role": if it.role == ItemRole::User { "user" } else { "assistant" }, "content": content_of(it)}))
            .collect();
        for item in &req.extra_input {
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    let args: Value = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or_else(|| json!({}));
                    msgs.push(json!({"role": "assistant", "content": [{"type": "tool_use", "id": item.get("call_id").cloned().unwrap_or(Value::Null), "name": item.get("name").cloned().unwrap_or(Value::Null), "input": args}]}));
                }
                Some("function_call_output") => msgs.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": item.get("call_id").cloned().unwrap_or(Value::Null), "content": item.get("output").cloned().unwrap_or(Value::Null)}]})),
                _ => {}
            }
        }
        body.insert("messages".into(), Value::Array(msgs));
        let mut tools = Vec::new();
        if req.tools.web_search.is_some() {
            tools.push(json!({"type": "web_search_20250305", "name": "web_search"}));
        }
        if req.tools.code_interpreter.is_some() {
            tools.push(json!({"type": "code_execution_20250522", "name": "code_execution"}));
        }
        if req.tools.knowledge {
            tools.push(json!({"name": super::KNOWLEDGE_TOOL, "description": super::KNOWLEDGE_DESCRIPTION, "input_schema": super::knowledge_parameters()}));
        }
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        if let Some(v) = req.api_params.temperature { body.insert("temperature".into(), json!(v)); }
        if let Some(v) = req.api_params.top_p { body.insert("top_p".into(), json!(v)); }
        if !req.api_params.stop.is_empty() { body.insert("stop_sequences".into(), json!(req.api_params.stop)); }
        body.insert("metadata".into(), json!({"user_id": req.user}));
        Value::Object(body)
    }

    fn parse_event(&self, state: &mut ParseState, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(data) else { return out };
        let name = event.map(str::to_owned).unwrap_or_else(|| v.get("type").and_then(Value::as_str).unwrap_or_default().to_owned());
        match name.as_str() {
            "message_start" => {
                if let Some(m) = v.get("message") {
                    state.response_id = m.get("id").and_then(Value::as_str).map(str::to_owned);
                    if let Some(u) = m.get("usage") { state.usage = Some(usage_of(u)); }
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                let ty = block.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
                match ty.as_str() {
                    "server_tool_use" => {
                        let tool = block.get("name").and_then(Value::as_str).unwrap_or_default();
                        let mapped = if tool == "code_execution" { "code_interpreter" } else { "web_search" };
                        state.block_kinds.insert(idx, mapped.to_owned());
                        out.push(LlmEvent::ToolStart { name: mapped.into(), details: json!({}) });
                    }
                    "tool_use" => {
                        let tool = block.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                        let shown = if tool == "search_knowledge" || tool == "load_files" { tool.clone() } else { "unknown_tool".to_owned() };
                        out.push(LlmEvent::ToolStart { name: shown, details: json!({}) });
                        let id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                        state.tool_index.insert(idx, (id, tool, String::new()));
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(d) = v.get("delta") {
                    match d.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(t) = d.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                                state.text.push_str(t);
                                out.push(LlmEvent::TextDelta(t.to_owned()));
                            }
                        }
                        Some("input_json_delta") => {
                            if let (Some(p), Some(e)) = (d.get("partial_json").and_then(Value::as_str), state.tool_index.get_mut(&idx)) {
                                e.2.push_str(p);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(kind) = state.block_kinds.remove(&idx)
                    && kind == "code_interpreter"
                {
                    out.push(LlmEvent::ToolDone { name: kind, details: json!({}) });
                }
                if let Some((id, name, args)) = state.tool_index.remove(&idx) {
                    out.push(LlmEvent::FunctionCall { name, call_id: id, arguments: args });
                }
            }
            "message_delta" => {
                if let Some(u) = v.get("usage") {
                    let mut cur = state.usage.unwrap_or_default();
                    if let Some(o) = u.get("output_tokens").and_then(Value::as_i64) { cur.output_tokens = o; }
                    state.usage = Some(cur);
                }
                if let Some(sr) = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                    state.finish_reason = Some(sr.to_owned());
                }
            }
            "message_stop" => {
                let incomplete = (state.finish_reason.as_deref() == Some("max_tokens")).then(|| "max_tokens".to_owned());
                out.push(LlmEvent::Completed(Completion { response_id: state.response_id.clone(), usage: state.usage, incomplete_reason: incomplete }));
            }
            "error" => out.push(LlmEvent::Failed(LlmFailure::provider(error_message(&v).unwrap_or_default()))),
            _ => {}
        }
        out
    }

    fn parse_complete(&self, body: &Value) -> Result<(String, Option<Usage>), LlmFailure> {
        if body.get("type").and_then(Value::as_str) == Some("error") {
            return Err(LlmFailure::provider(error_message(body).unwrap_or_default()));
        }
        let text: String = body
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join(""))
            .unwrap_or_default();
        Ok((text, body.get("usage").map(usage_of)))
    }

    fn extra_headers(&self) -> Vec<(&'static str, &'static str)> {
        vec![("anthropic-version", "2023-06-01")]
    }
}
