//! Anthropic Messages adapter: drops `file_search`, maps `web_search` / `code_interpreter`
//! to Anthropic server tools; images use the secondary (Anthropic Files) copy.

use std::collections::HashMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::{ContentPart, LlmRequest, ProviderError, ProviderErrorCode, ProviderEvent, ToolSpec};
use crate::domain::sanitize::sanitize_provider_message;

/// Builds the Messages body.
#[must_use]
pub fn build_request(req: &LlmRequest) -> Value {
    let messages: Vec<Value> = req
        .input
        .iter()
        .map(|m| {
            let parts: Vec<Value> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text(t) => Some(json!({"type": "text", "text": t})),
                    ContentPart::Image { secondary_file_id: Some(id), .. } => {
                        Some(json!({"type": "image", "source": {"type": "file", "file_id": id}}))
                    }
                    ContentPart::Image { .. } => None,
                })
                .collect();
            json!({"role": m.role.as_str(), "content": parts})
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("system".into(), json!(req.instructions));
    }
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(req.stream));
    body.insert("metadata".into(), json!({"user_id": req.user}));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::WebSearch { .. } => Some(json!({"type": "web_search_20250305", "name": "web_search"})),
            ToolSpec::CodeInterpreter { .. } => {
                Some(json!({"type": "code_execution_20250522", "name": "code_execution"}))
            }
            ToolSpec::FileSearch { .. } => None,
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    for (k, v) in [("temperature", req.api_params.temperature), ("top_p", req.api_params.top_p)] {
        if let Some(v) = v {
            body.insert(k.to_owned(), json!(v));
        }
    }
    Value::Object(body)
}

/// Stateful translator.
#[derive(Debug, Default)]
pub struct AnthropicTranslator {
    usage: UsageTokens,
    id: Option<String>,
    stop_reason: Option<String>,
    blocks: HashMap<u64, String>,
    done: bool,
}

impl AnthropicTranslator {
    /// Translates one frame.
    pub fn on_frame(&mut self, event: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return Vec::new() };
        let name = event
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .or_else(|| v.get("type").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match name.as_str() {
            "message_start" => {
                let msg = v.get("message");
                self.id = msg.and_then(|m| m.get("id")).and_then(Value::as_str).map(str::to_owned);
                if let Some(u) = msg.and_then(|m| m.get("usage")) {
                    self.usage.input_tokens = u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                    self.usage.cache_read_input_tokens =
                        u.get("cache_read_input_tokens").and_then(Value::as_i64).unwrap_or(0);
                    self.usage.cache_write_input_tokens =
                        u.get("cache_creation_input_tokens").and_then(Value::as_i64).unwrap_or(0);
                }
                Vec::new()
            }
            "content_block_start" => {
                let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                let ty = block.get("type").and_then(Value::as_str).unwrap_or_default();
                let tool = block.get("name").and_then(Value::as_str).unwrap_or_default();
                match (ty, tool) {
                    ("server_tool_use", "web_search") => {
                        self.blocks.insert(idx, "web_search".into());
                        vec![ProviderEvent::ToolStart { name: "web_search".into(), details: json!({}) }]
                    }
                    ("server_tool_use", "code_execution") => {
                        self.blocks.insert(idx, "code_interpreter".into());
                        vec![ProviderEvent::ToolStart { name: "code_interpreter".into(), details: json!({}) }]
                    }
                    ("tool_use", n) => {
                        let mapped = match n {
                            "search_knowledge" | "load_files" => n.to_owned(),
                            _ => "unknown_tool".to_owned(),
                        };
                        vec![ProviderEvent::ToolStart { name: mapped, details: json!({}) }]
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let d = v.get("delta");
                if d.and_then(|d| d.get("type")).and_then(Value::as_str) == Some("text_delta") {
                    let t = d.and_then(|d| d.get("text")).and_then(Value::as_str).unwrap_or_default();
                    if !t.is_empty() {
                        return vec![ProviderEvent::TextDelta(t.to_owned())];
                    }
                }
                Vec::new()
            }
            "content_block_stop" => match self.blocks.remove(&idx) {
                Some(name) => vec![ProviderEvent::ToolDone { name, details: json!({}) }],
                None => Vec::new(),
            },
            "message_delta" => {
                if let Some(r) = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_owned());
                }
                if let Some(o) = v.get("usage").and_then(|u| u.get("output_tokens")).and_then(Value::as_i64) {
                    self.usage.output_tokens = o;
                }
                Vec::new()
            }
            "message_stop" => {
                self.done = true;
                vec![ProviderEvent::Completed {
                    usage: Some(self.usage),
                    response_id: self.id.take(),
                    incomplete_reason: (self.stop_reason.as_deref() == Some("max_tokens"))
                        .then(|| "max_tokens".to_owned()),
                    citations: Vec::new(),
                }]
            }
            "error" => {
                self.done = true;
                let msg = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Provider returned an error");
                vec![ProviderEvent::Failed(ProviderError {
                    code: ProviderErrorCode::ProviderError,
                    message: sanitize_provider_message(msg),
                    usage: None,
                })]
            }
            _ => Vec::new(),
        }
    }
}

/// Parses a non-streaming body.
#[must_use]
pub fn parse_completion(body: &Value) -> (String, Option<UsageTokens>) {
    let text = body
        .get("content")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let usage = body.get("usage").map(|u| UsageTokens {
        input_tokens: u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0),
        output_tokens: u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0),
        ..UsageTokens::default()
    });
    (text, usage)
}
