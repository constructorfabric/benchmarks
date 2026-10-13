//! Anthropic Messages adapter (`kind: anthropic_messages`): drops
//! `file_search`, maps `web_search` / `code_interpreter` to server tools.

use serde_json::{Value, json};

use super::sse_parser::SseEvent;
use super::{
    Adapter, CompletionResult, DeltaKind, LlmRequest, ProviderEvent, Role, ToolSpec,
    merge_extra_body, parse_error_payload, parse_usage,
};

#[derive(Debug, Default)]
pub struct AnthropicMessages {
    usage: mini_chat_sdk::UsageTokens,
    stop_reason: Option<String>,
    response_id: Option<String>,
    open_blocks: Vec<(u64, String)>,
    /// Open client `tool_use` blocks: (index, id, name, input JSON).
    tool_uses: Vec<(u64, String, String, String)>,
}

impl Adapter for AnthropicMessages {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let messages: Vec<Value> = req
            .input
            .iter()
            .map(|m| {
                if m.role == Role::User && !m.secondary_image_file_ids.is_empty() {
                    let mut content: Vec<Value> = m
                        .secondary_image_file_ids
                        .iter()
                        .map(|id| json!({ "type": "image", "source": { "type": "file", "file_id": id } }))
                        .collect();
                    content.push(json!({ "type": "text", "text": m.text }));
                    json!({ "role": "user", "content": content })
                } else {
                    json!({ "role": m.role.as_str(), "content": m.text })
                }
            })
            .collect();
        let mut messages = messages;
        for x in &req.tool_exchanges {
            let input: Value = serde_json::from_str(&x.arguments).unwrap_or_else(|_| json!({}));
            messages.push(json!({
                "role": "assistant",
                "content": [{ "type": "tool_use", "id": x.call_id, "name": x.name, "input": input }],
            }));
            messages.push(json!({
                "role": "user",
                "content": [{ "type": "tool_result", "tool_use_id": x.call_id, "content": x.output }],
            }));
        }
        let mut body = json!({
            "model": req.provider_model_id,
            "messages": messages,
            "max_tokens": req.max_output_tokens,
            "stream": req.stream,
            "metadata": { "user_id": req.user },
        });
        if !req.instructions.is_empty() {
            body["system"] = Value::String(req.instructions.clone());
        }
        let tools: Vec<Value> = req
            .tools
            .iter()
            .filter_map(|t| match t {
                ToolSpec::WebSearch { .. } => {
                    Some(json!({ "type": "web_search_20250305", "name": "web_search" }))
                }
                ToolSpec::CodeInterpreter { .. } => {
                    Some(json!({ "type": "code_execution_20250522", "name": "code_execution" }))
                }
                ToolSpec::SearchKnowledge => Some(json!({
                    "name": "search_knowledge",
                    "description": "Search the organization knowledge base.",
                    "input_schema": { "type": "object", "properties": { "query": { "type": "string" }, "top_k": { "type": "integer" } }, "required": ["query"] }
                })),
                ToolSpec::FileSearch { .. } => None,
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(t) = req.api_params.temperature {
            body["temperature"] = Value::from(t);
        }
        if let Some(t) = req.api_params.top_p {
            body["top_p"] = Value::from(t);
        }
        if !req.api_params.stop.is_empty() {
            body["stop_sequences"] = Value::from(req.api_params.stop.clone());
        }
        let _ = merge_extra_body;
        body
    }

    fn translate(&mut self, ev: &SseEvent) -> Vec<ProviderEvent> {
        let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
        let name = ev
            .event
            .clone()
            .unwrap_or_else(|| data.get("type").and_then(Value::as_str).unwrap_or_default().to_owned());
        match name.as_str() {
            "message_start" => {
                if let Some(id) = data.pointer("/message/id").and_then(Value::as_str) {
                    self.response_id = Some(id.to_owned());
                }
                if let Some(u) = data.pointer("/message/usage").and_then(parse_usage) {
                    self.usage.input_tokens = u.input_tokens;
                    self.usage.cache_read_input_tokens = u.cache_read_input_tokens;
                    self.usage.cache_write_input_tokens = u.cache_write_input_tokens;
                }
                Vec::new()
            }
            "content_block_start" => {
                let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = data.get("content_block").cloned().unwrap_or(Value::Null);
                let bty = block.get("type").and_then(Value::as_str).unwrap_or_default();
                let bname = block.get("name").and_then(Value::as_str).unwrap_or_default();
                if bty == "tool_use" {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                    self.tool_uses.push((idx, id, bname.to_owned(), String::new()));
                }
                let tool = match (bty, bname) {
                    ("server_tool_use", "code_execution") => Some("code_interpreter".to_owned()),
                    ("server_tool_use", "web_search") => Some("web_search".to_owned()),
                    ("tool_use", "search_knowledge" | "load_files") => Some(bname.to_owned()),
                    ("tool_use", _) => Some("unknown_tool".to_owned()),
                    _ => None,
                };
                match tool {
                    Some(t) => {
                        self.open_blocks.push((idx, t.clone()));
                        vec![ProviderEvent::ToolStart {
                            name: t,
                            details: json!({}),
                        }]
                    }
                    None => Vec::new(),
                }
            }
            "content_block_delta" => match data.pointer("/delta/type").and_then(Value::as_str) {
                Some("text_delta") => vec![ProviderEvent::Delta {
                    kind: DeltaKind::Text,
                    text: data
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }],
                Some("input_json_delta") => {
                    let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let part = data.pointer("/delta/partial_json").and_then(Value::as_str).unwrap_or_default();
                    if let Some(t) = self.tool_uses.iter_mut().find(|t| t.0 == idx) {
                        t.3.push_str(part);
                    }
                    Vec::new()
                }
                Some("thinking_delta") => vec![ProviderEvent::Delta {
                    kind: DeltaKind::Reasoning,
                    text: data
                        .pointer("/delta/thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }],
                _ => Vec::new(),
            },
            "content_block_stop" => {
                let idx = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(p) = self.tool_uses.iter().position(|t| t.0 == idx) {
                    let (_, call_id, name, input) = self.tool_uses.remove(p);
                    if let Some(o) = self.open_blocks.iter().position(|(i, _)| *i == idx) {
                        self.open_blocks.remove(o);
                    }
                    let arguments = if input.trim().is_empty() { "{}".to_owned() } else { input };
                    return vec![ProviderEvent::FunctionCall { call_id, name, arguments }];
                }
                let pos = self.open_blocks.iter().position(|(i, _)| *i == idx);
                match pos.map(|p| self.open_blocks.remove(p)) {
                    Some((_, n)) if n == "code_interpreter" || n == "web_search" => {
                        vec![ProviderEvent::ToolDone {
                            name: n,
                            details: json!({}),
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                if let Some(r) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_owned());
                }
                if let Some(o) = data.pointer("/usage/output_tokens").and_then(Value::as_i64) {
                    self.usage.output_tokens = o;
                }
                Vec::new()
            }
            "message_stop" => vec![ProviderEvent::Completed {
                response_id: self.response_id.take(),
                usage: Some(self.usage),
                incomplete_reason: self
                    .stop_reason
                    .as_deref()
                    .filter(|r| *r == "max_tokens")
                    .map(ToOwned::to_owned),
            }],
            "error" => {
                let (code, message) = parse_error_payload(&ev.data);
                vec![ProviderEvent::Failed {
                    code,
                    message,
                    usage: None,
                }]
            }
            _ => Vec::new(),
        }
    }

    fn parse_completion(&self, body: &Value) -> Result<CompletionResult, String> {
        if body.get("type").and_then(Value::as_str) == Some("error") {
            return Err(parse_error_payload(&body.to_string()).1);
        }
        let text = body
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default();
        Ok(CompletionResult {
            text,
            usage: body.get("usage").and_then(parse_usage),
        })
    }
}
