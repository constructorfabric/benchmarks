//! Chat Completions adapter (`openai_chat_completions`). Built-in tools are
//! dropped; images are not sent (no file-id input in this API).

use serde_json::{Map, Value, json};

use super::sse::SseFrame;
use super::{ChatRequest, ProviderErrorKind, ProviderEvent, ProviderUsage, apply_api_params};

#[must_use]
pub fn build_body(req: &ChatRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({"role": "system", "content": req.instructions}));
    }
    for m in &req.input {
        messages.push(json!({"role": m.role, "content": m.text}));
    }
    let mut body = Map::new();
    body.insert("model".into(), Value::String(req.model.clone()));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), Value::Bool(req.stream));
    if req.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    body.insert("max_completion_tokens".into(), Value::from(req.max_output_tokens));
    body.insert("user".into(), Value::String(req.user.clone()));
    if !req.api_params.stop.is_empty() {
        body.insert("stop".into(), json!(req.api_params.stop));
    }
    apply_api_params(&mut body, &req.api_params, true);
    Value::Object(body)
}

fn parse_usage(v: &Value) -> Option<ProviderUsage> {
    let u = v.as_object()?;
    let n = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some(ProviderUsage {
        input_tokens: n("prompt_tokens"),
        output_tokens: n("completion_tokens"),
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

#[derive(Debug, Default)]
pub struct ChatCompletionsDecoder {
    id: Option<String>,
    usage: Option<ProviderUsage>,
    finish: Option<String>,
    terminal: bool,
}

impl ChatCompletionsDecoder {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    pub fn on_frame(&mut self, frame: &SseFrame) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if frame.data.trim() == "[DONE]" {
            out.extend(self.finish());
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(&frame.data) else {
            return out;
        };
        if let Some(e) = v.get("error") {
            self.terminal = true;
            out.push(ProviderEvent::Failed {
                kind: ProviderErrorKind::ProviderError,
                message: e.get("message").and_then(Value::as_str).unwrap_or("Provider returned an error").to_owned(),
                usage: None,
            });
            return out;
        }
        if self.id.is_none()
            && let Some(id) = v.get("id").and_then(Value::as_str)
        {
            self.id = Some(id.to_owned());
            out.push(ProviderEvent::ResponseId(id.to_owned()));
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()).and_then(parse_usage) {
            self.usage = Some(u);
        }
        if let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| c.first()) {
            let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
            if let Some(t) = delta.get("content").and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(ProviderEvent::TextDelta(t.to_owned()));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let name = c.get("function").and_then(|f| f.get("name")).and_then(Value::as_str);
                    if let Some(name) = name {
                        out.push(ProviderEvent::ToolStart {
                            name: "function_call".into(),
                            details: json!({"index": c.get("index"), "call_id": c.get("id"), "name": name}),
                        });
                    }
                }
            }
            if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish = Some(f.to_owned());
                if f == "tool_calls" {
                    self.terminal = true;
                    out.push(ProviderEvent::Failed {
                        kind: ProviderErrorKind::UnexpectedToolUse,
                        message: "The model requested a tool that this turn does not handle".into(),
                        usage: self.usage,
                    });
                }
            }
        }
        out
    }

    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        let incomplete = matches!(self.finish.as_deref(), Some("length")).then(|| "max_tokens".to_owned());
        vec![ProviderEvent::Completed {
            response_id: self.id.clone(),
            usage: self.usage,
            incomplete_reason: incomplete,
        }]
    }
}
