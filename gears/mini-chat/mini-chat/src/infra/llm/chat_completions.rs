//! Chat Completions adapter (`openai_chat_completions`): keeps function
//! tools, drops `file_search`, `web_search` and `code_interpreter`.

use serde_json::{Map, Value, json};

use super::openai_responses::{extract_error, merge_extra_body, parse_usage};
use super::types::{LlmPart, LlmRequest, LlmRole, LlmTool, ProviderEvent, ProviderFailureKind, ProviderUsage};
use crate::domain::sanitize::sanitize_provider_message;

/// Build the Chat Completions request body.
#[must_use]
pub fn build_body(req: &LlmRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({"role": "system", "content": req.instructions}));
    }
    for m in &req.messages {
        match m.role {
            LlmRole::User => {
                let text: String = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        LlmPart::Text(t) => Some(t.as_str()),
                        LlmPart::Image { .. } => None,
                    })
                    .collect();
                messages.push(json!({"role": "user", "content": text}));
            }
            LlmRole::Assistant => messages.push(json!({"role": "assistant", "content": m.joined_text()})),
        }
    }
    messages.extend(req.extra_input.iter().cloned());
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.provider_model_id));
    body.insert("messages".to_owned(), Value::Array(messages));
    body.insert("stream".to_owned(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".to_owned(), json!({"include_usage": true}));
    }
    body.insert("max_completion_tokens".to_owned(), json!(req.max_output_tokens));
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            LlmTool::Function { name, description, parameters } => Some(json!({
                "type": "function",
                "function": {"name": name, "description": description, "parameters": parameters},
            })),
            _ => None,
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
    if let Some(v) = p.frequency_penalty {
        body.insert("frequency_penalty".to_owned(), json!(v));
    }
    if let Some(v) = p.presence_penalty {
        body.insert("presence_penalty".to_owned(), json!(v));
    }
    if !p.stop.is_empty() {
        body.insert("stop".to_owned(), json!(p.stop));
    }
    if let Some(e) = &p.reasoning_effort {
        body.insert("reasoning_effort".to_owned(), json!(e));
    }
    body.insert("user".to_owned(), json!(req.user));
    merge_extra_body(&mut body, p.extra_body.as_ref());
    Value::Object(body)
}

#[derive(Debug, Default, Clone)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

/// Incremental translator of Chat Completions SSE chunks.
#[derive(Debug, Default)]
pub struct ChatCompletionsParser {
    usage: Option<ProviderUsage>,
    finish_reason: Option<String>,
    response_id: Option<String>,
    tool_calls: Vec<ToolCallAcc>,
    finished: bool,
}

impl ChatCompletionsParser {
    /// Translate one `data:` payload.
    pub fn on_event(&mut self, data: &str) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if data.trim() == "[DONE]" {
            out.extend(self.finish());
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return out;
        };
        if v.get("error").is_some() {
            let (code, message) = extract_error(&v);
            self.finished = true;
            out.push(ProviderEvent::Failed {
                kind: ProviderFailureKind::ProviderError,
                message: sanitize_provider_message(message.as_deref().unwrap_or("Provider returned an error")),
                provider_code: code,
                usage: None,
                response_id: self.response_id.clone(),
            });
            return out;
        }
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = parse_usage(u);
        }
        if let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| c.first()) {
            if let Some(delta) = choice.get("delta") {
                if let Some(t) = delta.get("content").and_then(Value::as_str)
                    && !t.is_empty()
                {
                    out.push(ProviderEvent::TextDelta(t.to_owned()));
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let idx = c.get("index").and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok()).unwrap_or(0);
                        if self.tool_calls.len() <= idx {
                            self.tool_calls.resize(idx + 1, ToolCallAcc::default());
                        }
                        let acc = &mut self.tool_calls[idx];
                        if let Some(id) = c.get("id").and_then(Value::as_str) {
                            id.clone_into(&mut acc.id);
                        }
                        if let Some(f) = c.get("function") {
                            if let Some(n) = f.get("name").and_then(Value::as_str) {
                                acc.name.push_str(n);
                            }
                            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                                acc.arguments.push_str(a);
                            }
                        }
                        if !acc.started && !acc.name.is_empty() {
                            acc.started = true;
                            out.push(ProviderEvent::ToolStart {
                                name: "function_call".to_owned(),
                                details: json!({"index": idx, "call_id": acc.id, "name": acc.name}),
                            });
                        }
                    }
                }
            }
            if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(r.to_owned());
            }
        }
        out
    }

    /// Finalize at `[DONE]` or end of stream.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = Vec::new();
        if self.finish_reason.as_deref() == Some("tool_calls")
            && let Some(c) = self.tool_calls.first().cloned()
        {
            out.push(ProviderEvent::ToolDone {
                name: "function_call".to_owned(),
                details: json!({"call_id": c.id, "name": c.name, "arguments": c.arguments}),
            });
            out.push(ProviderEvent::FunctionCall {
                raw_item: json!({
                    "role": "assistant",
                    "tool_calls": [{"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.arguments}}],
                }),
                call_id: c.id,
                name: c.name,
                arguments: c.arguments,
            });
            return out;
        }
        if self.finish_reason.is_none() && self.usage.is_none() {
            return out;
        }
        let incomplete_reason = match self.finish_reason.as_deref() {
            Some("length") => Some("max_tokens".to_owned()),
            Some("content_filter") => Some("content_filter".to_owned()),
            _ => None,
        };
        out.push(ProviderEvent::Completed {
            usage: self.usage,
            response_id: self.response_id.clone(),
            incomplete_reason,
            citations: Vec::new(),
            output_text: None,
        });
        out
    }
}

/// Parse a non-streaming Chat Completions body: `(text, usage)`.
#[must_use]
pub fn parse_non_streaming(v: &Value) -> (String, Option<ProviderUsage>) {
    let text = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (text, v.get("usage").and_then(parse_usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_chunks_and_done() {
        let mut p = ChatCompletionsParser::default();
        let ev = p.on_event(r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"Hi"}}]}"#);
        assert_eq!(ev, vec![ProviderEvent::TextDelta("Hi".into())]);
        assert!(p.on_event(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#).is_empty());
        assert!(p.on_event(r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#).is_empty());
        let ev = p.on_event("[DONE]");
        assert!(matches!(&ev[0], ProviderEvent::Completed { usage: Some(u), incomplete_reason: None, .. } if u.input_tokens == 3 && u.output_tokens == 2));
    }

    #[test]
    fn tool_call_finishes_with_function_call() {
        let mut p = ChatCompletionsParser::default();
        let ev = p.on_event(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"search_knowledge","arguments":"{\"q"}}]}}]}"#);
        assert!(matches!(&ev[0], ProviderEvent::ToolStart { name, .. } if name == "function_call"));
        p.on_event(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"uery\":\"x\"}"}}]},"finish_reason":"tool_calls"}]}"#);
        let ev = p.finish();
        assert!(matches!(&ev[1], ProviderEvent::FunctionCall { arguments, .. } if arguments == r#"{"query":"x"}"#));
    }

    #[test]
    fn body_drops_builtin_tools() {
        let req = LlmRequest {
            provider_model_id: "m".into(),
            instructions: "sys".into(),
            messages: vec![super::super::types::LlmMessage::text(LlmRole::User, "q")],
            max_output_tokens: 10,
            tools: vec![LlmTool::WebSearch { context_size: mini_chat_sdk::WebSearchContextSize::Low }],
            max_tool_calls: 2,
            api_params: mini_chat_sdk::ModelApiParams::default(),
            user: "u".into(),
            metadata: super::super::types::LlmMetadata {
                tenant_id: String::new(),
                user_id: String::new(),
                chat_id: String::new(),
                request_type: "chat",
                feature: "none".into(),
            },
            stream: true,
            extra_input: vec![],
        };
        let b = build_body(&req);
        assert!(b.get("tools").is_none());
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["max_completion_tokens"], 10);
    }
}
