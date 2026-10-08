//! Anthropic Messages API adapter. Drops `file_search`; maps `web_search` and
//! `code_interpreter` to Anthropic server tools.

use std::collections::HashMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use crate::infra::llm::types::{
    ContentPart, InputItem, LlmRequest, ProviderErrorCode, ProviderEvent, ProviderFailure, ToolSpec,
};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub fn build_body(req: &LlmRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for item in &req.input {
        match item {
            InputItem::Message { role, content } => {
                let parts: Vec<Value> = content
                    .iter()
                    .filter_map(|p| {
                        match p {
                        ContentPart::Text(t) => Some(json!({ "type": "text", "text": t })),
                        ContentPart::Image {
                            secondary_file_id, ..
                        } => secondary_file_id.as_ref().map(|id| {
                            json!({ "type": "image", "source": { "type": "file", "file_id": id } })
                        }),
                    }
                    })
                    .collect();
                messages.push(json!({ "role": role.as_str(), "content": parts }));
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let input: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
                messages.push(json!({
                    "role": "assistant",
                    "content": [{ "type": "tool_use", "id": call_id, "name": name, "input": input }],
                }));
            }
            InputItem::FunctionCallOutput { call_id, output } => messages.push(json!({
                "role": "user",
                "content": [{ "type": "tool_result", "tool_use_id": call_id, "content": output }],
            })),
        }
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("system".into(), json!(req.instructions));
    }
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(req.stream));
    body.insert("metadata".into(), json!({ "user_id": req.user }));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::FileSearch { .. } => None,
            ToolSpec::WebSearch { .. } => Some(json!({
                "type": "web_search_20250305",
                "name": "web_search",
            })),
            ToolSpec::CodeInterpreter { .. } => Some(json!({
                "type": "code_execution_20250522",
                "name": "code_execution",
            })),
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => Some(
                json!({ "name": name, "description": description, "input_schema": parameters }),
            ),
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    let p = &req.api_params;
    if let Some(v) = p.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".into(), json!(v));
    }
    if !p.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(p.stop));
    }
    Value::Object(body)
}

#[derive(Debug, Default)]
struct Block {
    kind: String,
    name: String,
    id: String,
    json: String,
}

/// Streaming parser of Anthropic Messages events.
#[derive(Debug, Default)]
pub struct AnthropicParser {
    blocks: HashMap<u64, Block>,
    usage: UsageTokens,
    response_id: Option<String>,
    stop_reason: Option<String>,
    tool_uses: Vec<(String, String, String)>,
    pub done: bool,
}

impl AnthropicParser {
    pub fn push(&mut self, event: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        if self.done {
            return vec![];
        }
        let v: Value = serde_json::from_str(data).unwrap_or(Value::Null);
        let name = event
            .filter(|n| !n.is_empty() && *n != "message")
            .map(str::to_owned)
            .or_else(|| v.get("type").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        let mut out = Vec::new();
        match name.as_str() {
            "message_start" => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                self.response_id = m.get("id").and_then(Value::as_str).map(str::to_owned);
                if let Some(u) = m.get("usage") {
                    self.merge_usage(u);
                }
            }
            "content_block_start" => {
                let b = v.get("content_block").cloned().unwrap_or(Value::Null);
                let kind = b
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let tool_name = b
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let id = b.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
                match kind.as_str() {
                    "server_tool_use" => {
                        let mapped = if tool_name == "code_execution" {
                            "code_interpreter".to_owned()
                        } else {
                            tool_name.clone()
                        };
                        out.push(ProviderEvent::ToolStart {
                            name: mapped,
                            details: json!({}),
                        });
                    }
                    "tool_use" => {
                        let ev_name = match tool_name.as_str() {
                            "search_knowledge" | "load_files" => tool_name.clone(),
                            _ => "unknown_tool".to_owned(),
                        };
                        out.push(ProviderEvent::ToolStart {
                            name: ev_name,
                            details: json!({}),
                        });
                    }
                    _ => {}
                }
                self.blocks.insert(
                    idx,
                    Block {
                        kind,
                        name: tool_name,
                        id,
                        json: String::new(),
                    },
                );
            }
            "content_block_delta" => {
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = d.get("text").and_then(Value::as_str)
                            && !t.is_empty()
                        {
                            out.push(ProviderEvent::TextDelta(t.to_owned()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = d.get("thinking").and_then(Value::as_str) {
                            out.push(ProviderEvent::ReasoningDelta(t.to_owned()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(b) = self.blocks.get_mut(&idx)
                            && let Some(p) = d.get("partial_json").and_then(Value::as_str)
                        {
                            b.json.push_str(p);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(b) = self.blocks.remove(&idx) {
                    if b.kind == "server_tool_use" && b.name == "code_execution" {
                        out.push(ProviderEvent::ToolDone {
                            name: "code_interpreter".into(),
                            details: json!({}),
                        });
                    } else if b.kind == "tool_use" {
                        let args = if b.json.is_empty() {
                            "{}".to_owned()
                        } else {
                            b.json
                        };
                        self.tool_uses.push((b.id, b.name, args));
                    }
                }
            }
            "message_delta" => {
                if let Some(sr) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(sr.to_owned());
                }
                if let Some(u) = v.get("usage") {
                    self.merge_usage(u);
                }
            }
            "message_stop" => {
                self.done = true;
                if let Some((id, name, args)) = self.tool_uses.first().cloned() {
                    out.push(ProviderEvent::FunctionCall {
                        call_id: id,
                        name,
                        arguments: args,
                    });
                } else {
                    let incomplete_reason = match self.stop_reason.as_deref() {
                        Some("max_tokens") => Some("max_tokens".to_owned()),
                        Some("refusal") => Some("content_filter".to_owned()),
                        _ => None,
                    };
                    out.push(ProviderEvent::Completed {
                        response_id: self.response_id.clone(),
                        usage: Some(self.usage),
                        citations: vec![],
                        incomplete_reason,
                    });
                }
            }
            "error" => {
                self.done = true;
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or(data);
                let kind = v
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let code = if kind == "rate_limit_error" {
                    ProviderErrorCode::RateLimited
                } else {
                    ProviderErrorCode::ProviderError
                };
                out.push(ProviderEvent::Failed(ProviderFailure::new(code, msg)));
            }
            _ => {}
        }
        out
    }

    fn merge_usage(&mut self, u: &Value) {
        let get = |k: &str| u.get(k).and_then(Value::as_i64);
        if let Some(n) = get("input_tokens") {
            self.usage.input_tokens = n
                + get("cache_read_input_tokens").unwrap_or(0)
                + get("cache_creation_input_tokens").unwrap_or(0);
        }
        if let Some(n) = get("cache_read_input_tokens") {
            self.usage.cache_read_input_tokens = n;
        }
        if let Some(n) = get("cache_creation_input_tokens") {
            self.usage.cache_write_input_tokens = n;
        }
        if let Some(n) = get("output_tokens") {
            self.usage.output_tokens = n;
        }
    }
}

/// Text and usage of a non-streaming message.
pub fn parse_message(v: &Value) -> (String, Option<UsageTokens>) {
    let mut text = String::new();
    if let Some(parts) = v.get("content").and_then(Value::as_array) {
        for p in parts {
            if p.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = p.get("text").and_then(Value::as_str)
            {
                text.push_str(t);
            }
        }
    }
    let usage = v.get("usage").map(|u| {
        let get = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
        UsageTokens {
            input_tokens: get("input_tokens")
                + get("cache_read_input_tokens")
                + get("cache_creation_input_tokens"),
            output_tokens: get("output_tokens"),
            cache_read_input_tokens: get("cache_read_input_tokens"),
            cache_write_input_tokens: get("cache_creation_input_tokens"),
            reasoning_tokens: 0,
        }
    });
    (text, usage)
}
