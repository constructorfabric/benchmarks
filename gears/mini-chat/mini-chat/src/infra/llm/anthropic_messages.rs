//! Anthropic Messages adapter (`anthropic_messages`). `file_search` is
//! dropped; `web_search` and `code_interpreter` map to server tools.

use serde_json::{Map, Value, json};

use super::sse::SseFrame;
use super::{ChatRequest, ProviderErrorKind, ProviderEvent, ProviderUsage};

#[must_use]
pub fn build_body(req: &ChatRequest) -> Value {
    let messages: Vec<Value> = req
        .input
        .iter()
        .map(|m| json!({"role": m.role, "content": [{"type": "text", "text": m.text}]}))
        .collect();
    let mut body = Map::new();
    body.insert("model".into(), Value::String(req.model.clone()));
    if !req.instructions.is_empty() {
        body.insert("system".into(), Value::String(req.instructions.clone()));
    }
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), Value::from(req.max_output_tokens));
    body.insert("stream".into(), Value::Bool(req.stream));
    body.insert("metadata".into(), json!({"user_id": req.user}));
    let mut tools = Vec::new();
    if req.tools.web_search.is_some() {
        tools.push(json!({"type": "web_search_20250305", "name": "web_search"}));
    }
    if req.tools.code_interpreter.is_some() {
        tools.push(json!({"type": "code_execution_20250522", "name": "code_execution"}));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(t) = req.api_params.temperature {
        body.insert("temperature".into(), Value::from(t));
    }
    if let Some(t) = req.api_params.top_p {
        body.insert("top_p".into(), Value::from(t));
    }
    if !req.api_params.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(req.api_params.stop));
    }
    Value::Object(body)
}

#[derive(Debug, Default)]
pub struct AnthropicDecoder {
    id: Option<String>,
    usage: ProviderUsage,
    stop_reason: Option<String>,
    blocks: std::collections::HashMap<u64, String>,
    terminal: bool,
}

impl AnthropicDecoder {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    pub fn on_frame(&mut self, frame: &SseFrame) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(&frame.data) else {
            return out;
        };
        let name = frame
            .event
            .clone()
            .unwrap_or_else(|| v.get("type").and_then(Value::as_str).unwrap_or_default().to_owned());
        match name.as_str() {
            "message_start" => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                if let Some(id) = m.get("id").and_then(Value::as_str) {
                    self.id = Some(id.to_owned());
                    out.push(ProviderEvent::ResponseId(id.to_owned()));
                }
                if let Some(u) = m.get("usage") {
                    self.usage.input_tokens = u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                    self.usage.cache_read_input_tokens = u.get("cache_read_input_tokens").and_then(Value::as_i64).unwrap_or(0);
                    self.usage.cache_write_input_tokens =
                        u.get("cache_creation_input_tokens").and_then(Value::as_i64).unwrap_or(0);
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                let btype = block.get("type").and_then(Value::as_str).unwrap_or_default();
                let bname = block.get("name").and_then(Value::as_str).unwrap_or_default();
                match (btype, bname) {
                    ("server_tool_use", "web_search") => {
                        self.blocks.insert(idx, "web_search".into());
                        out.push(ProviderEvent::ToolStart { name: "web_search".into(), details: json!({}) });
                    }
                    ("server_tool_use", "code_execution") => {
                        self.blocks.insert(idx, "code_interpreter".into());
                        out.push(ProviderEvent::ToolStart { name: "code_interpreter".into(), details: json!({}) });
                    }
                    ("tool_use", n) => {
                        let tool = if n == "search_knowledge" || n == "load_files" { n } else { "unknown_tool" };
                        out.push(ProviderEvent::ToolStart { name: tool.into(), details: json!({}) });
                        self.terminal = true;
                        out.push(ProviderEvent::Failed {
                            kind: ProviderErrorKind::UnexpectedToolUse,
                            message: "The model requested a tool that this turn does not handle".into(),
                            usage: None,
                        });
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                if d.get("type").and_then(Value::as_str) == Some("text_delta")
                    && let Some(t) = d.get("text").and_then(Value::as_str)
                {
                    out.push(ProviderEvent::TextDelta(t.to_owned()));
                }
            }
            "content_block_stop" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(name) = self.blocks.remove(&idx) {
                    out.push(ProviderEvent::ToolDone { name, details: json!({}) });
                }
            }
            "message_delta" => {
                if let Some(o) = v.get("usage").and_then(|u| u.get("output_tokens")).and_then(Value::as_i64) {
                    self.usage.output_tokens = o;
                }
                if let Some(r) = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_owned());
                }
            }
            "message_stop" => {
                self.terminal = true;
                out.push(ProviderEvent::Completed {
                    response_id: self.id.clone(),
                    usage: Some(self.usage),
                    incomplete_reason: matches!(self.stop_reason.as_deref(), Some("max_tokens")).then(|| "max_tokens".to_owned()),
                });
            }
            "error" => {
                self.terminal = true;
                out.push(ProviderEvent::Failed {
                    kind: ProviderErrorKind::ProviderError,
                    message: v
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("Provider returned an error")
                        .to_owned(),
                    usage: None,
                });
            }
            _ => {}
        }
        out
    }
}
