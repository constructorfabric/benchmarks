//! Anthropic Messages adapter (`anthropic_messages`).
//!
//! Drops `file_search`; maps `web_search` and `code_interpreter` to the
//! Anthropic server tools; function tools are sent as client tools. Images are
//! sent by their secondary Anthropic Files id (an image without one is
//! dropped). No citations are parsed.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::types::{
    Completion, ContentPart, FunctionItem, InputRole, LlmEvent, LlmRequest, ProviderErrorKind,
    ProviderFailure, ToolSpec, Usage,
};

/// `anthropic-version` header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// `anthropic-beta` value enabling the Files API.
pub const ANTHROPIC_FILES_BETA: &str = "files-api-2025-04-14";

fn content_json(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(json!({"type": "text", "text": t})),
            ContentPart::Image {
                secondary_file_id: Some(id),
                ..
            } => Some(json!({"type": "image", "source": {"type": "file", "file_id": id}})),
            ContentPart::Image { .. } => None,
        })
        .collect()
}

/// `true` when the request references Anthropic Files (needs the beta
/// header).
#[must_use]
pub fn uses_files(req: &LlmRequest) -> bool {
    req.input.iter().any(|m| {
        m.content.iter().any(|p| {
            matches!(
                p,
                ContentPart::Image {
                    secondary_file_id: Some(_),
                    ..
                }
            )
        })
    })
}

/// Build a Messages API request body.
#[must_use]
pub fn build_request(req: &LlmRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    for m in &req.input {
        let role = match m.role {
            InputRole::User => "user",
            InputRole::Assistant => "assistant",
        };
        let content = content_json(&m.content);
        if content.is_empty() {
            continue;
        }
        messages.push(json!({"role": role, "content": content}));
    }
    for item in &req.function_items {
        match item {
            FunctionItem::Call {
                call_id,
                name,
                arguments,
            } => {
                let input: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
                messages.push(json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": call_id, "name": name, "input": input}]}));
            }
            FunctionItem::Output { call_id, output } => messages.push(json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": call_id, "content": output}]
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
    body.insert("metadata".into(), json!({"user_id": req.user}));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::FileSearch { .. } => None,
            ToolSpec::WebSearch { .. } => {
                Some(json!({"type": "web_search_20250305", "name": "web_search"}))
            }
            ToolSpec::CodeInterpreter { .. } => {
                Some(json!({"type": "code_execution_20250522", "name": "code_execution"}))
            }
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => {
                Some(json!({"name": name, "description": description, "input_schema": parameters}))
            }
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

#[derive(Debug, Clone)]
enum Block {
    Text,
    ServerTool(String),
    ClientTool {
        id: String,
        name: String,
        input: String,
    },
    Other,
}

/// Translator of Anthropic Messages SSE events.
#[derive(Debug, Default)]
pub struct AnthropicTranslator {
    terminal: bool,
    response_id: Option<String>,
    usage: Usage,
    stop_reason: Option<String>,
    blocks: HashMap<u64, Block>,
    pending_call: Option<(String, String, String)>,
}

fn tool_name(name: &str) -> &'static str {
    match name {
        "search_knowledge" => "search_knowledge",
        "load_files" => "load_files",
        _ => "unknown_tool",
    }
}

impl AnthropicTranslator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read_usage(&mut self, u: &Value) {
        let n = |k: &str| u.get(k).and_then(Value::as_i64);
        if let Some(v) = n("input_tokens") {
            self.usage.input_tokens = v;
        }
        if let Some(v) = n("output_tokens") {
            self.usage.output_tokens = v;
        }
        if let Some(v) = n("cache_read_input_tokens") {
            self.usage.cache_read_input_tokens = v;
        }
        if let Some(v) = n("cache_creation_input_tokens") {
            self.usage.cache_write_input_tokens = v;
        }
    }

    /// Translate one SSE event.
    #[allow(clippy::too_many_lines)]
    pub fn on_event(&mut self, event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        if self.terminal {
            return Vec::new();
        }
        let Ok(json) = serde_json::from_str::<Value>(data.trim()) else {
            return Vec::new();
        };
        let name = match event {
            Some(n) if !n.is_empty() && n != "message" => n.to_owned(),
            _ => json
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        let index = json.get("index").and_then(Value::as_u64).unwrap_or(0);
        match name.as_str() {
            "message_start" => {
                if let Some(msg) = json.get("message") {
                    self.response_id = msg.get("id").and_then(Value::as_str).map(str::to_owned);
                    if let Some(u) = msg.get("usage") {
                        self.read_usage(u);
                    }
                }
                Vec::new()
            }
            "content_block_start" => {
                let block = json.get("content_block").unwrap_or(&Value::Null);
                let ty = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mut out = Vec::new();
                let b = match ty {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(Value::as_str)
                            && !t.is_empty()
                        {
                            out.push(LlmEvent::TextDelta(t.to_owned()));
                        }
                        Block::Text
                    }
                    "server_tool_use" => {
                        let n = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let mapped = match n {
                            "web_search" => Some("web_search"),
                            "code_execution"
                            | "bash_code_execution"
                            | "text_editor_code_execution" => Some("code_interpreter"),
                            _ => None,
                        };
                        match mapped {
                            Some(m) => {
                                out.push(LlmEvent::ToolStart {
                                    name: m.to_owned(),
                                    details: json!({}),
                                });
                                Block::ServerTool(m.to_owned())
                            }
                            None => Block::Other,
                        }
                    }
                    "tool_use" => {
                        let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                        let n = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        out.push(LlmEvent::ToolStart {
                            name: tool_name(n).to_owned(),
                            details: json!({}),
                        });
                        Block::ClientTool {
                            id: id.to_owned(),
                            name: n.to_owned(),
                            input: String::new(),
                        }
                    }
                    _ => Block::Other,
                };
                self.blocks.insert(index, b);
                out
            }
            "content_block_delta" => {
                let delta = json.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => delta
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                        .map(|t| LlmEvent::TextDelta(t.to_owned()))
                        .into_iter()
                        .collect(),
                    Some("input_json_delta") => {
                        if let Some(Block::ClientTool { input, .. }) = self.blocks.get_mut(&index)
                            && let Some(p) = delta.get("partial_json").and_then(Value::as_str)
                        {
                            input.push_str(p);
                        }
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_stop" => match self.blocks.remove(&index) {
                Some(Block::ServerTool(name)) => vec![LlmEvent::ToolDone {
                    name,
                    details: json!({}),
                }],
                Some(Block::ClientTool { id, name, input }) => {
                    if self.pending_call.is_none() {
                        let args = if input.trim().is_empty() {
                            "{}".to_owned()
                        } else {
                            input
                        };
                        self.pending_call = Some((id, name, args));
                    }
                    Vec::new()
                }
                _ => Vec::new(),
            },
            "message_delta" => {
                if let Some(r) = json
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(r.to_owned());
                }
                if let Some(u) = json.get("usage") {
                    self.read_usage(u);
                }
                Vec::new()
            }
            "message_stop" => self.finalize(),
            "error" => {
                self.terminal = true;
                let err = json.get("error").unwrap_or(&json);
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error")
                    .to_owned();
                let code = err.get("type").and_then(Value::as_str).map(str::to_owned);
                let kind = if code.as_deref() == Some("rate_limit_error") {
                    ProviderErrorKind::RateLimited {
                        retry_after_secs: None,
                    }
                } else {
                    ProviderErrorKind::Provider
                };
                vec![LlmEvent::Failed(ProviderFailure {
                    kind,
                    message,
                    provider_code: code,
                    usage: None,
                    response_id: self.response_id.clone(),
                })]
            }
            _ => Vec::new(),
        }
    }

    fn finalize(&mut self) -> Vec<LlmEvent> {
        self.terminal = true;
        if self.stop_reason.as_deref() == Some("tool_use")
            && let Some((call_id, name, arguments)) = self.pending_call.take()
        {
            return vec![LlmEvent::FunctionCall {
                call_id,
                name,
                arguments,
            }];
        }
        let incomplete_reason = match self.stop_reason.as_deref() {
            Some("max_tokens") => Some("max_tokens".to_owned()),
            Some("refusal") => Some("content_filter".to_owned()),
            _ => None,
        };
        vec![LlmEvent::Completed(Completion {
            usage: Some(self.usage),
            response_id: self.response_id.clone(),
            incomplete_reason,
        })]
    }

    /// End of stream without `message_stop`.
    pub fn finish(&mut self) -> Option<LlmEvent> {
        if self.terminal {
            return None;
        }
        self.terminal = true;
        Some(LlmEvent::Failed(ProviderFailure::provider(
            "provider stream ended without a terminal event",
        )))
    }
}

/// Text and usage of a non-streaming Messages result.
#[must_use]
pub fn parse_complete(json: &Value) -> (String, Option<Usage>) {
    let text = json
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let usage = json.get("usage").map(|u| {
        let mut t = AnthropicTranslator::new();
        t.read_usage(u);
        t.usage
    });
    (text, usage)
}
