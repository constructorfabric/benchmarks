//! Chat Completions adapter: keeps function tools only (none in P1 without knowledge search).

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::responses::apply_api_params;
use super::{ContentPart, LlmRequest, ProviderError, ProviderErrorCode, ProviderEvent};
use crate::domain::sanitize::sanitize_provider_message;

/// Builds the Chat Completions body.
#[must_use]
pub fn build_request(req: &LlmRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({"role": "system", "content": req.instructions}));
    }
    for m in &req.input {
        let text: Vec<&str> = m
            .parts
            .iter()
            .filter_map(|p| if let ContentPart::Text(t) = p { Some(t.as_str()) } else { None })
            .collect();
        messages.push(json!({"role": m.role.as_str(), "content": text.join("\n")}));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    body.insert("max_tokens".into(), json!(req.max_output_tokens));
    body.insert("user".into(), json!(req.user));
    apply_api_params(&mut body, req);
    Value::Object(body)
}

/// Parses Chat Completions usage.
#[must_use]
pub fn parse_usage(v: &Value) -> Option<UsageTokens> {
    let u = v.as_object()?;
    Some(UsageTokens {
        input_tokens: u.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
        output_tokens: u.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0),
        cache_read_input_tokens: u
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        cache_write_input_tokens: 0,
        reasoning_tokens: u
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
    })
}

/// Stateful translator.
#[derive(Debug, Default)]
pub struct ChatTranslator {
    usage: Option<UsageTokens>,
    id: Option<String>,
    finish_length: bool,
    done: bool,
}

impl ChatTranslator {
    /// Translates one frame.
    pub fn on_frame(&mut self, data: &str) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        if data.trim() == "[DONE]" {
            self.done = true;
            return vec![ProviderEvent::Completed {
                usage: self.usage,
                response_id: self.id.take(),
                incomplete_reason: self.finish_length.then(|| "max_tokens".to_owned()),
                citations: Vec::new(),
            }];
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return Vec::new() };
        if let Some(err) = v.get("error") {
            self.done = true;
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("Provider returned an error");
            return vec![ProviderEvent::Failed(ProviderError {
                code: ProviderErrorCode::ProviderError,
                message: sanitize_provider_message(msg),
                usage: None,
            })];
        }
        if self.id.is_none() {
            self.id = v.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()).and_then(parse_usage) {
            self.usage = Some(u);
        }
        let mut out = Vec::new();
        if let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| c.first()) {
            if let Some(t) = choice.get("delta").and_then(|d| d.get("content")).and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(ProviderEvent::TextDelta(t.to_owned()));
            }
            if let Some(calls) = choice.get("delta").and_then(|d| d.get("tool_calls")).and_then(Value::as_array) {
                for c in calls {
                    out.push(ProviderEvent::ToolStart {
                        name: "function_call".into(),
                        details: json!({
                            "index": c.get("index"),
                            "call_id": c.get("id"),
                            "name": c.get("function").and_then(|f| f.get("name")),
                        }),
                    });
                }
            }
            if choice.get("finish_reason").and_then(Value::as_str) == Some("length") {
                self.finish_length = true;
            }
        }
        out
    }

    /// Terminal event when the stream ended without `[DONE]` but after a finish.
    pub fn finish(&mut self) -> Option<ProviderEvent> {
        if self.done {
            return None;
        }
        self.done = true;
        None
    }
}

/// Parses a non-streaming body.
#[must_use]
pub fn parse_completion(body: &Value) -> (String, Option<UsageTokens>) {
    let text = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (text, body.get("usage").and_then(parse_usage))
}
