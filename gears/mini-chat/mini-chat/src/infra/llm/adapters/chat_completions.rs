//! Chat Completions API adapter. Drops built-in tools (`file_search`,
//! `web_search`, `code_interpreter`); keeps function tools.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::openai_responses::{apply_api_params, parse_usage};
use crate::infra::llm::types::{
    ContentPart, InputItem, LlmRequest, ProviderErrorCode, ProviderEvent, ProviderFailure, ToolSpec,
};

pub fn build_body(req: &LlmRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": req.instructions }));
    }
    for item in &req.input {
        match item {
            InputItem::Message { role, content } => {
                let only_text = content.iter().all(|p| matches!(p, ContentPart::Text(_)));
                if only_text {
                    let text: Vec<&str> = content
                        .iter()
                        .filter_map(|p| match p {
                            ContentPart::Text(t) => Some(t.as_str()),
                            ContentPart::Image { .. } => None,
                        })
                        .collect();
                    messages.push(json!({ "role": role.as_str(), "content": text.join("\n") }));
                } else {
                    let parts: Vec<Value> = content
                        .iter()
                        .map(|p| match p {
                            ContentPart::Text(t) => json!({ "type": "text", "text": t }),
                            ContentPart::Image { file_id, .. } => {
                                json!({ "type": "file", "file": { "file_id": file_id } })
                            }
                        })
                        .collect();
                    messages.push(json!({ "role": role.as_str(), "content": parts }));
                }
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments },
                }],
            })),
            InputItem::FunctionCallOutput { call_id, output } => messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output,
            })),
        }
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
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
            } => Some(json!({
                "type": "function",
                "function": { "name": name, "description": description, "parameters": parameters },
            })),
            _ => None,
        })
        .collect();
    if !functions.is_empty() {
        body.insert("tools".into(), Value::Array(functions));
    }
    if !req.api_params.stop.is_empty() {
        body.insert("stop".into(), json!(req.api_params.stop));
    }
    if let Some(effort) = &req.api_params.reasoning_effort {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    apply_api_params(&mut body, req, true);
    Value::Object(body)
}

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

/// Streaming parser of Chat Completions chunks.
#[derive(Debug, Default)]
pub struct ChatCompletionsParser {
    calls: BTreeMap<u64, PendingCall>,
    finish_reason: Option<String>,
    usage: Option<mini_chat_sdk::UsageTokens>,
    response_id: Option<String>,
    pub done: bool,
}

impl ChatCompletionsParser {
    fn finish(&mut self) -> Vec<ProviderEvent> {
        self.done = true;
        let mut out = Vec::new();
        if self.finish_reason.as_deref() == Some("tool_calls") || !self.calls.is_empty() {
            for (_, c) in std::mem::take(&mut self.calls) {
                out.push(ProviderEvent::ToolDone {
                    name: "function_call".into(),
                    details: json!({ "call_id": c.id, "name": c.name, "arguments": c.arguments }),
                });
                out.push(ProviderEvent::FunctionCall {
                    call_id: c.id,
                    name: c.name,
                    arguments: c.arguments,
                });
            }
            return out;
        }
        let incomplete_reason = match self.finish_reason.as_deref() {
            Some("length") => Some("max_tokens".to_owned()),
            Some("content_filter") => Some("content_filter".to_owned()),
            _ => None,
        };
        out.push(ProviderEvent::Completed {
            response_id: self.response_id.clone(),
            usage: self.usage,
            citations: vec![],
            incomplete_reason,
        });
        out
    }

    pub fn push(&mut self, _event: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        if self.done {
            return vec![];
        }
        if data.trim() == "[DONE]" {
            return self.finish();
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return vec![],
        };
        if v.get("error").is_some() {
            self.done = true;
            let msg = v
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or(data);
            return vec![ProviderEvent::Failed(ProviderFailure::new(
                ProviderErrorCode::ProviderError,
                msg,
            ))];
        }
        if self.response_id.is_none() {
            self.response_id = v.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        if let Some(u) = parse_usage(v.get("usage").filter(|u| u.is_object())) {
            self.usage = Some(u);
        }
        let mut out = Vec::new();
        if let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
            if let Some(t) = delta.get("content").and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(ProviderEvent::TextDelta(t.to_owned()));
            }
            if let Some(t) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(ProviderEvent::ReasoningDelta(t.to_owned()));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let idx = c.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let entry = self.calls.entry(idx).or_default();
                    let is_new = entry.id.is_empty() && entry.name.is_empty();
                    if let Some(id) = c.get("id").and_then(Value::as_str) {
                        id.clone_into(&mut entry.id);
                    }
                    if let Some(f) = c.get("function") {
                        if let Some(n) = f.get("name").and_then(Value::as_str) {
                            entry.name.push_str(n);
                        }
                        if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                            entry.arguments.push_str(a);
                        }
                    }
                    if is_new {
                        out.push(ProviderEvent::ToolStart {
                            name: "function_call".into(),
                            details: json!({ "index": idx, "call_id": entry.id, "name": entry.name }),
                        });
                    }
                }
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(fr.to_owned());
            }
        }
        out
    }

    /// End of stream without `[DONE]`.
    pub fn eof(&mut self) -> Vec<ProviderEvent> {
        if self.done || self.finish_reason.is_none() {
            return vec![];
        }
        self.finish()
    }
}

/// Text and usage of a non-streaming completion.
pub fn parse_completion(v: &Value) -> (String, Option<mini_chat_sdk::UsageTokens>) {
    let text = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (text, parse_usage(v.get("usage")))
}
