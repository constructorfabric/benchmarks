//! Chat Completions and Anthropic Messages adapters (text streaming; ADR-0005
//! notes they have no E2E coverage). Built-in tools are dropped by these
//! adapters; function tools are not used in P1 (knowledge search is off).

use serde_json::{Map, Value, json};

use super::openai_responses::{error_message, merge_extra_body, parse_usage};
use super::sse::SseFrame;
use super::types::{
    InputPart, InputRole, LlmEvent, LlmRequest, ProviderErrorKind, ProviderFailure,
};

fn text_of(parts: &[InputPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            InputPart::Text(t) => Some(t.as_str()),
            InputPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Chat Completions request body.
#[must_use]
pub fn build_chat_completions(req: &LlmRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({"role": "system", "content": req.instructions}));
    }
    for m in &req.input {
        let role = match m.role {
            InputRole::User => "user",
            InputRole::Assistant => "assistant",
        };
        messages.push(json!({"role": role, "content": text_of(&m.parts)}));
    }
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("stream".to_owned(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".to_owned(), json!({"include_usage": true}));
    }
    body.insert("max_completion_tokens".to_owned(), json!(req.max_output_tokens));
    body.insert("user".to_owned(), json!(req.user));
    let p = &req.api_params;
    if let Some(v) = p.temperature {
        body.insert("temperature".to_owned(), json!(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".to_owned(), json!(v));
    }
    if !p.stop.is_empty() {
        body.insert("stop".to_owned(), json!(p.stop));
    }
    merge_extra_body(&mut body, p.extra_body.as_ref());
    Value::Object(body)
}

/// Translator of Chat Completions chunks.
#[derive(Debug, Default)]
pub struct ChatCompletionsTranslator {
    usage: Option<mini_chat_sdk::UsageTokens>,
    finish_reason: Option<String>,
    id: Option<String>,
    terminal: bool,
}

impl ChatCompletionsTranslator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Translates one frame.
    pub fn on_frame(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        if self.terminal {
            return Vec::new();
        }
        let data = frame.data.trim();
        if data == "[DONE]" {
            return self.finish();
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        if v.get("error").is_some() {
            self.terminal = true;
            return vec![LlmEvent::Failed(ProviderFailure::error(
                error_message(&v).unwrap_or_else(|| "provider error".to_owned()),
            ))];
        }
        if self.id.is_none() {
            self.id = v.get("id").and_then(Value::as_str).map(ToOwned::to_owned);
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = parse_usage(u);
        }
        let mut out = Vec::new();
        if let Some(choice) = v.get("choices").and_then(|c| c.get(0)) {
            if let Some(t) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                out.push(LlmEvent::TextDelta(t.to_owned()));
            }
            if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(r.to_owned());
            }
        }
        out
    }

    /// Terminal event at `[DONE]` or end of stream.
    pub fn finish(&mut self) -> Vec<LlmEvent> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        let incomplete = match self.finish_reason.as_deref() {
            Some("length") => Some("max_tokens".to_owned()),
            Some("content_filter") => Some("content_filter".to_owned()),
            _ => None,
        };
        vec![LlmEvent::Completed {
            usage: self.usage,
            response_id: self.id.clone(),
            incomplete_reason: incomplete,
        }]
    }
}

/// Anthropic Messages request body.
#[must_use]
pub fn build_anthropic(req: &LlmRequest) -> Value {
    let messages: Vec<Value> = req
        .input
        .iter()
        .map(|m| {
            let role = match m.role {
                InputRole::User => "user",
                InputRole::Assistant => "assistant",
            };
            json!({"role": role, "content": text_of(&m.parts)})
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("system".to_owned(), json!(req.instructions));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("max_tokens".to_owned(), json!(req.max_output_tokens));
    body.insert("stream".to_owned(), json!(req.stream));
    body.insert("metadata".to_owned(), json!({"user_id": req.user}));
    if let Some(v) = req.api_params.temperature {
        body.insert("temperature".to_owned(), json!(v));
    }
    Value::Object(body)
}

/// Translator of Anthropic Messages events.
#[derive(Debug, Default)]
pub struct AnthropicTranslator {
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_write: i64,
    stop_reason: Option<String>,
    id: Option<String>,
    terminal: bool,
}

impl AnthropicTranslator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Translates one frame.
    pub fn on_frame(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        if self.terminal {
            return Vec::new();
        }
        let Ok(v) = serde_json::from_str::<Value>(frame.data.trim()) else {
            return Vec::new();
        };
        let ty = frame
            .event
            .clone()
            .or_else(|| v.get("type").and_then(Value::as_str).map(ToOwned::to_owned))
            .unwrap_or_default();
        match ty.as_str() {
            "message_start" => {
                let msg = v.get("message").unwrap_or(&Value::Null);
                self.id = msg.get("id").and_then(Value::as_str).map(ToOwned::to_owned);
                if let Some(u) = msg.get("usage") {
                    self.input_tokens = u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                    self.cache_read = u
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.cache_write = u
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                }
                Vec::new()
            }
            "content_block_delta" => v
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .map(|t| vec![LlmEvent::TextDelta(t.to_owned())])
                .unwrap_or_default(),
            "content_block_start" => {
                let block = v.get("content_block").unwrap_or(&Value::Null);
                match block.get("type").and_then(Value::as_str) {
                    Some("server_tool_use") => {
                        let name = match block.get("name").and_then(Value::as_str) {
                            Some("web_search") => "web_search",
                            Some("code_execution") => "code_interpreter",
                            Some(other) => other,
                            None => "unknown_tool",
                        };
                        vec![LlmEvent::ToolStart {
                            name: name.to_owned(),
                            details: json!({}),
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                if let Some(o) = v
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_i64)
                {
                    self.output_tokens = o;
                }
                if let Some(r) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(r.to_owned());
                }
                Vec::new()
            }
            "message_stop" => self.finish(),
            "error" => {
                self.terminal = true;
                let kind = if v
                    .get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    == Some("rate_limit_error")
                {
                    ProviderErrorKind::RateLimited
                } else {
                    ProviderErrorKind::ProviderError
                };
                vec![LlmEvent::Failed(ProviderFailure {
                    kind,
                    message: error_message(&v).unwrap_or_else(|| "provider error".to_owned()),
                    usage: None,
                    response_id: None,
                })]
            }
            _ => Vec::new(),
        }
    }

    /// Terminal event.
    pub fn finish(&mut self) -> Vec<LlmEvent> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        let incomplete = (self.stop_reason.as_deref() == Some("max_tokens")).then(|| "max_tokens".to_owned());
        vec![LlmEvent::Completed {
            usage: Some(mini_chat_sdk::UsageTokens {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cache_read_input_tokens: self.cache_read,
                cache_write_input_tokens: self.cache_write,
                reasoning_tokens: 0,
            }),
            response_id: self.id.clone(),
            incomplete_reason: incomplete,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(event: Option<&str>, data: &str) -> SseFrame {
        SseFrame {
            event: event.map(ToOwned::to_owned),
            data: data.to_owned(),
        }
    }

    #[test]
    fn chat_completions_stream() {
        let mut t = ChatCompletionsTranslator::new();
        let mut ev = t.on_frame(&f(None, r#"{"id":"c1","choices":[{"delta":{"content":"Hi"}}]}"#));
        ev.extend(t.on_frame(&f(
            None,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
        )));
        ev.extend(t.on_frame(&f(None, "[DONE]")));
        assert_eq!(ev[0], LlmEvent::TextDelta("Hi".to_owned()));
        assert!(
            matches!(&ev[1], LlmEvent::Completed { usage: Some(u), .. } if u.input_tokens == 3 && u.output_tokens == 1)
        );
    }

    #[test]
    fn anthropic_stream() {
        let mut t = AnthropicTranslator::new();
        let mut ev = t.on_frame(&f(
            Some("message_start"),
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":4}}}"#,
        ));
        ev.extend(t.on_frame(&f(
            Some("content_block_delta"),
            r#"{"delta":{"type":"text_delta","text":"Yo"}}"#,
        )));
        ev.extend(t.on_frame(&f(
            Some("message_delta"),
            r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        )));
        ev.extend(t.on_frame(&f(Some("message_stop"), "{}")));
        assert_eq!(ev[0], LlmEvent::TextDelta("Yo".to_owned()));
        assert!(
            matches!(&ev[1], LlmEvent::Completed { usage: Some(u), .. } if u.input_tokens == 4 && u.output_tokens == 2)
        );
    }
}
