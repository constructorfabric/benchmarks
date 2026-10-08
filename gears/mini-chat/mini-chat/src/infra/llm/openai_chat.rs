//! Chat Completions adapter (`openai_chat_completions`).
//!
//! Drops `file_search`, `web_search` and `code_interpreter` and keeps
//! function tools. Tool events of function calls are reported as
//! `function_call` (`start` with `index`, `call_id`, `name`; `done` with
//! `call_id`, `name`, `arguments`).

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::openai_responses::{apply_api_params, parse_stream_error, parse_usage};
use super::types::{
    Completion, ContentPart, FunctionItem, InputRole, LlmEvent, LlmRequest, ProviderErrorKind,
    ProviderFailure, ToolSpec,
};

fn message_json(role: InputRole, content: &[ContentPart]) -> Value {
    match role {
        InputRole::User => {
            let has_image = content
                .iter()
                .any(|p| matches!(p, ContentPart::Image { .. }));
            if has_image {
                let parts: Vec<Value> = content
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text(t) => json!({"type": "text", "text": t}),
                        ContentPart::Image { file_id, .. } => {
                            json!({"type": "file", "file": {"file_id": file_id}})
                        }
                    })
                    .collect();
                json!({"role": "user", "content": parts})
            } else {
                json!({"role": "user", "content": joined_text(content)})
            }
        }
        InputRole::Assistant => json!({"role": "assistant", "content": joined_text(content)}),
    }
}

fn joined_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a Chat Completions request body.
#[must_use]
pub fn build_request(req: &LlmRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({"role": "system", "content": req.instructions}));
    }
    for m in &req.input {
        messages.push(message_json(m.role, &m.content));
    }
    for item in &req.function_items {
        match item {
            FunctionItem::Call {
                call_id,
                name,
                arguments,
            } => messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": call_id, "type": "function",
                                "function": {"name": name, "arguments": arguments}}]
            })),
            FunctionItem::Output { call_id, output } => messages.push(json!({
                "role": "tool", "tool_call_id": call_id, "content": output
            })),
        }
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
    body.insert("user".into(), json!(req.user));
    let functions: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => Some(json!({"type": "function", "function": {
                "name": name, "description": description, "parameters": parameters}})),
            _ => None,
        })
        .collect();
    if !functions.is_empty() {
        body.insert("tools".into(), Value::Array(functions));
    }
    apply_api_params(&mut body, req, true, false);
    Value::Object(body)
}

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

/// Translator of Chat Completions stream chunks.
#[derive(Debug, Default)]
pub struct ChatTranslator {
    terminal: bool,
    finish_reason: Option<String>,
    usage: Option<super::types::Usage>,
    response_id: Option<String>,
    calls: BTreeMap<u64, PendingCall>,
}

impl ChatTranslator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn finalize(&mut self) -> Vec<LlmEvent> {
        self.terminal = true;
        let mut out = Vec::new();
        let calls = std::mem::take(&mut self.calls);
        let mut first_call = None;
        for (_, call) in calls {
            out.push(LlmEvent::ToolDone {
                name: "function_call".to_owned(),
                details: json!({"call_id": call.id, "name": call.name, "arguments": call.arguments}),
            });
            if first_call.is_none() {
                first_call = Some(call);
            }
        }
        if let Some(call) = first_call {
            out.push(LlmEvent::FunctionCall {
                call_id: call.id,
                name: call.name,
                arguments: call.arguments,
            });
            return out;
        }
        let incomplete_reason = match self.finish_reason.as_deref() {
            Some("length") => Some("max_tokens".to_owned()),
            Some("content_filter") => Some("content_filter".to_owned()),
            _ => None,
        };
        out.push(LlmEvent::Completed(Completion {
            usage: self.usage,
            response_id: self.response_id.clone(),
            incomplete_reason,
        }));
        out
    }

    /// Translate one SSE data chunk.
    pub fn on_event(&mut self, _event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        if self.terminal {
            return Vec::new();
        }
        let trimmed = data.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        if trimmed == "[DONE]" {
            return self.finalize();
        }
        let Ok(json) = serde_json::from_str::<Value>(trimmed) else {
            return Vec::new();
        };
        if json.get("error").is_some() {
            let (code, message) = parse_stream_error(&json, trimmed);
            self.terminal = true;
            return vec![LlmEvent::Failed(ProviderFailure {
                kind: ProviderErrorKind::Provider,
                message,
                provider_code: code,
                usage: None,
                response_id: self.response_id.clone(),
            })];
        }
        if let Some(id) = json.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(u) = json.get("usage").and_then(parse_usage) {
            self.usage = Some(u);
        }
        let mut out = Vec::new();
        if let Some(choices) = json.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let delta = choice.get("delta").unwrap_or(&Value::Null);
                if let Some(text) = delta.get("content").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    out.push(LlmEvent::TextDelta(text.to_owned()));
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let is_new = !self.calls.contains_key(&index);
                        let entry = self.calls.entry(index).or_default();
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            id.clone_into(&mut entry.id);
                        }
                        if let Some(f) = call.get("function") {
                            if let Some(n) = f.get("name").and_then(Value::as_str) {
                                entry.name.push_str(n);
                            }
                            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                                entry.arguments.push_str(a);
                            }
                        }
                        if is_new {
                            out.push(LlmEvent::ToolStart {
                                name: "function_call".to_owned(),
                                details: json!({"index": index, "call_id": entry.id, "name": entry.name}),
                            });
                        }
                    }
                }
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    self.finish_reason = Some(reason.to_owned());
                }
            }
        }
        out
    }

    /// End of stream without `[DONE]`: finalize when a finish reason was
    /// seen, else a provider error.
    pub fn finish(&mut self) -> Option<LlmEvent> {
        if self.terminal {
            return None;
        }
        if self.finish_reason.is_some() {
            return self.finalize().pop();
        }
        self.terminal = true;
        Some(LlmEvent::Failed(ProviderFailure::provider(
            "provider stream ended without a terminal event",
        )))
    }
}

/// Text and usage of a non-streaming Chat Completions result.
#[must_use]
pub fn parse_complete(json: &Value) -> (String, Option<super::types::Usage>) {
    let text = json
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (text, json.get("usage").and_then(parse_usage))
}
