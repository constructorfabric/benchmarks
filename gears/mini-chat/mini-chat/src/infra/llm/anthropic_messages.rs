//! Anthropic Messages adapter (`anthropic_messages`): drops `file_search`,
//! maps `web_search` / `code_interpreter` to Anthropic server tools.

use serde_json::{Map, Value, json};

use super::types::{LlmPart, LlmRequest, LlmRole, LlmTool, ProviderEvent, ProviderFailureKind, ProviderUsage};
use crate::domain::sanitize::sanitize_provider_message;

/// Build the Messages API request body.
#[must_use]
pub fn build_body(req: &LlmRequest) -> Value {
    let mut messages = Vec::new();
    for m in &req.messages {
        match m.role {
            LlmRole::User => {
                let content: Vec<Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        LlmPart::Text(t) => Some(json!({"type": "text", "text": t})),
                        LlmPart::Image { secondary_file_id: Some(id), .. } => {
                            Some(json!({"type": "image", "source": {"type": "file", "file_id": id}}))
                        }
                        LlmPart::Image { secondary_file_id: None, .. } => None,
                    })
                    .collect();
                messages.push(json!({"role": "user", "content": content}));
            }
            LlmRole::Assistant => messages.push(json!({"role": "assistant", "content": m.joined_text()})),
        }
    }
    messages.extend(req.extra_input.iter().cloned());
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.provider_model_id));
    if !req.instructions.is_empty() {
        body.insert("system".to_owned(), json!(req.instructions));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("max_tokens".to_owned(), json!(req.max_output_tokens));
    body.insert("stream".to_owned(), json!(req.stream));
    body.insert("metadata".to_owned(), json!({"user_id": req.user}));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            LlmTool::FileSearch { .. } => None,
            LlmTool::WebSearch { .. } => Some(json!({"type": "web_search_20250305", "name": "web_search"})),
            LlmTool::CodeInterpreter { .. } => Some(json!({"type": "code_execution_20250522", "name": "code_execution"})),
            LlmTool::Function { name, description, parameters } => {
                Some(json!({"name": name, "description": description, "input_schema": parameters}))
            }
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    let p = &req.api_params;
    if let Some(v) = p.temperature {
        body.insert("temperature".to_owned(), json!(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".to_owned(), json!(v));
    }
    if !p.stop.is_empty() {
        body.insert("stop_sequences".to_owned(), json!(p.stop));
    }
    Value::Object(body)
}

#[derive(Debug, Default)]
struct Block {
    kind: String,
    name: String,
    id: String,
    input_json: String,
}

/// Incremental translator of Anthropic SSE events.
#[derive(Debug, Default)]
pub struct AnthropicParser {
    usage: ProviderUsage,
    response_id: Option<String>,
    stop_reason: Option<String>,
    blocks: Vec<Block>,
    pending_tool: Option<(String, String, String)>,
}

fn num(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_i64).unwrap_or(0)
}

impl AnthropicParser {
    /// Translate one event.
    pub fn on_event(&mut self, name: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return out;
        };
        let ev = name
            .filter(|n| !n.is_empty() && *n != "message")
            .map(str::to_owned)
            .or_else(|| v.get("type").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        match ev.as_str() {
            "message_start" => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                self.response_id = m.get("id").and_then(Value::as_str).map(str::to_owned);
                if let Some(u) = m.get("usage") {
                    self.usage.input_tokens = num(u.get("input_tokens"))
                        + num(u.get("cache_read_input_tokens"))
                        + num(u.get("cache_creation_input_tokens"));
                    self.usage.cache_read_input_tokens = num(u.get("cache_read_input_tokens"));
                    self.usage.cache_write_input_tokens = num(u.get("cache_creation_input_tokens"));
                    self.usage.output_tokens = num(u.get("output_tokens"));
                }
            }
            "content_block_start" => {
                let b = v.get("content_block").cloned().unwrap_or(Value::Null);
                let kind = b.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
                let bname = b.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
                let id = b.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                match (kind.as_str(), bname.as_str()) {
                    ("server_tool_use", "web_search") => out.push(ProviderEvent::ToolStart {
                        name: "web_search".to_owned(),
                        details: json!({}),
                    }),
                    ("server_tool_use", "code_execution") => out.push(ProviderEvent::ToolStart {
                        name: "code_interpreter".to_owned(),
                        details: json!({}),
                    }),
                    ("tool_use", n) => {
                        let shown = if matches!(n, "search_knowledge" | "load_files") { n } else { "unknown_tool" };
                        out.push(ProviderEvent::ToolStart { name: shown.to_owned(), details: json!({}) });
                    }
                    _ => {}
                }
                self.blocks.push(Block { kind, name: bname, id, input_json: String::new() });
            }
            "content_block_delta" => {
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = d.get("text").and_then(Value::as_str) {
                            out.push(ProviderEvent::TextDelta(t.to_owned()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(b), Some(p)) = (self.blocks.last_mut(), d.get("partial_json").and_then(Value::as_str)) {
                            b.input_json.push_str(p);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(b) = self.blocks.last() {
                    if b.kind == "server_tool_use" && b.name == "code_execution" {
                        out.push(ProviderEvent::ToolDone { name: "code_interpreter".to_owned(), details: json!({}) });
                    }
                    if b.kind == "server_tool_use" && b.name == "web_search" {
                        out.push(ProviderEvent::ToolDone { name: "web_search".to_owned(), details: json!({}) });
                    }
                    if b.kind == "tool_use" {
                        self.pending_tool = Some((b.id.clone(), b.name.clone(), b.input_json.clone()));
                    }
                }
            }
            "message_delta" => {
                if let Some(r) = v.get("delta").and_then(|d| d.get("stop_reason")).and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_owned());
                }
                if let Some(u) = v.get("usage")
                    && u.get("output_tokens").is_some()
                {
                    self.usage.output_tokens = num(u.get("output_tokens"));
                }
            }
            "message_stop" => {
                if self.stop_reason.as_deref() == Some("tool_use")
                    && let Some((id, n, args)) = self.pending_tool.take()
                {
                    out.push(ProviderEvent::FunctionCall {
                        raw_item: json!({"role": "assistant", "content": [{"type": "tool_use", "id": id, "name": n, "input": serde_json::from_str::<Value>(&args).unwrap_or(json!({}))}]}),
                        call_id: id,
                        name: n,
                        arguments: args,
                    });
                    return out;
                }
                let incomplete_reason = (self.stop_reason.as_deref() == Some("max_tokens")).then(|| "max_tokens".to_owned());
                out.push(ProviderEvent::Completed {
                    usage: Some(self.usage),
                    response_id: self.response_id.clone(),
                    incomplete_reason,
                    citations: Vec::new(),
                    output_text: None,
                });
            }
            "error" => {
                let e = v.get("error").cloned().unwrap_or(Value::Null);
                let ty = e.get("type").and_then(Value::as_str).unwrap_or_default();
                let kind = if ty == "rate_limit_error" {
                    ProviderFailureKind::RateLimited
                } else {
                    ProviderFailureKind::ProviderError
                };
                out.push(ProviderEvent::Failed {
                    kind,
                    message: sanitize_provider_message(e.get("message").and_then(Value::as_str).unwrap_or("Provider returned an error")),
                    provider_code: Some(ty.to_owned()),
                    usage: None,
                    response_id: self.response_id.clone(),
                });
            }
            _ => {}
        }
        out
    }
}

/// Parse a non-streaming Messages body: `(text, usage)`.
#[must_use]
pub fn parse_non_streaming(v: &Value) -> (String, Option<ProviderUsage>) {
    let mut text = String::new();
    if let Some(blocks) = v.get("content").and_then(Value::as_array) {
        for b in blocks {
            if b.get("type").and_then(Value::as_str) == Some("text") {
                text.push_str(b.get("text").and_then(Value::as_str).unwrap_or_default());
            }
        }
    }
    let usage = v.get("usage").map(|u| ProviderUsage {
        input_tokens: num(u.get("input_tokens")),
        output_tokens: num(u.get("output_tokens")),
        cache_read_input_tokens: num(u.get("cache_read_input_tokens")),
        cache_write_input_tokens: num(u.get("cache_creation_input_tokens")),
        reasoning_tokens: 0,
    });
    (text, usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_message_flow() {
        let mut p = AnthropicParser::default();
        p.on_event(Some("message_start"), r#"{"message":{"id":"msg_1","usage":{"input_tokens":7,"output_tokens":1}}}"#);
        p.on_event(Some("content_block_start"), r#"{"content_block":{"type":"text"}}"#);
        let ev = p.on_event(Some("content_block_delta"), r#"{"delta":{"type":"text_delta","text":"Hey"}}"#);
        assert_eq!(ev, vec![ProviderEvent::TextDelta("Hey".into())]);
        let ev = p.on_event(Some("content_block_start"), r#"{"content_block":{"type":"server_tool_use","name":"code_execution"}}"#);
        assert!(matches!(&ev[0], ProviderEvent::ToolStart { name, .. } if name == "code_interpreter"));
        let ev = p.on_event(Some("content_block_stop"), "{}");
        assert!(matches!(&ev[0], ProviderEvent::ToolDone { name, .. } if name == "code_interpreter"));
        p.on_event(Some("message_delta"), r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#);
        let ev = p.on_event(Some("message_stop"), "{}");
        assert!(matches!(&ev[0], ProviderEvent::Completed { usage: Some(u), .. } if u.input_tokens == 7 && u.output_tokens == 5));
    }

    #[test]
    fn overloaded_error_is_provider_error() {
        let mut p = AnthropicParser::default();
        let ev = p.on_event(Some("error"), r#"{"error":{"type":"overloaded_error","message":"Overloaded"}}"#);
        assert!(matches!(&ev[0], ProviderEvent::Failed { kind: ProviderFailureKind::ProviderError, .. }));
    }
}
