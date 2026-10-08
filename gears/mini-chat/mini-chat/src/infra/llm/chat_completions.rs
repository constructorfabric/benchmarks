//! Chat Completions adapter (`kind: openai_chat_completions`): drops the
//! built-in tools, keeps function tools.

use serde_json::{Value, json};

use super::sse_parser::SseEvent;
use super::{
    Adapter, CompletionResult, DeltaKind, LlmRequest, ProviderEvent, Role, ToolSpec,
    apply_sampling, merge_extra_body, parse_error_payload, parse_usage,
};

#[derive(Debug, Default)]
pub struct ChatCompletions {
    usage: Option<mini_chat_sdk::UsageTokens>,
    finish_reason: Option<String>,
    response_id: Option<String>,
    done: bool,
    /// Streamed function calls by index: (call id, name, arguments).
    calls: Vec<(u64, String, String, String)>,
}

impl Adapter for ChatCompletions {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut messages = Vec::new();
        if !req.instructions.is_empty() {
            messages.push(json!({ "role": "system", "content": req.instructions }));
        }
        for m in &req.input {
            if m.image_file_ids.is_empty() || m.role != Role::User {
                messages.push(json!({ "role": m.role.as_str(), "content": m.text }));
            } else {
                let mut content = vec![json!({ "type": "text", "text": m.text })];
                for id in &m.image_file_ids {
                    content.push(json!({ "type": "file", "file": { "file_id": id } }));
                }
                messages.push(json!({ "role": "user", "content": content }));
            }
        }
        for x in &req.tool_exchanges {
            messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{ "id": x.call_id, "type": "function", "function": { "name": x.name, "arguments": x.arguments } }],
            }));
            messages.push(json!({ "role": "tool", "tool_call_id": x.call_id, "content": x.output }));
        }
        let mut body = json!({
            "model": req.provider_model_id,
            "messages": messages,
            "stream": req.stream,
            "max_completion_tokens": req.max_output_tokens,
            "user": req.user,
        });
        if req.stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        let tools: Vec<Value> = req
            .tools
            .iter()
            .filter(|t| matches!(t, ToolSpec::SearchKnowledge))
            .map(|_| {
                json!({ "type": "function", "function": {
                    "name": "search_knowledge",
                    "description": "Search the organization knowledge base.",
                    "parameters": { "type": "object", "properties": { "query": { "type": "string" }, "top_k": { "type": "integer" } }, "required": ["query"] }
                }})
            })
            .collect();
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        apply_sampling(&mut body, &req.api_params, true);
        merge_extra_body(&mut body, &req.api_params);
        body
    }

    fn translate(&mut self, ev: &SseEvent) -> Vec<ProviderEvent> {
        if self.done {
            return Vec::new();
        }
        if ev.data.trim() == "[DONE]" {
            self.done = true;
            let incomplete = self
                .finish_reason
                .as_deref()
                .filter(|r| *r == "length")
                .map(|_| "max_tokens".to_owned());
            let mut out = Vec::new();
            for (_, call_id, name, arguments) in std::mem::take(&mut self.calls) {
                out.push(ProviderEvent::ToolDone {
                    name: "function_call".into(),
                    details: json!({ "call_id": call_id, "name": name, "arguments": arguments }),
                });
                out.push(ProviderEvent::FunctionCall { call_id, name, arguments });
            }
            out.push(ProviderEvent::Completed {
                response_id: self.response_id.take(),
                usage: self.usage.take(),
                incomplete_reason: incomplete,
            });
            return out;
        }
        let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
        if data.get("error").is_some() || ev.event.as_deref() == Some("error") {
            self.done = true;
            let (code, message) = parse_error_payload(&ev.data);
            return vec![ProviderEvent::Failed {
                code,
                message,
                usage: None,
            }];
        }
        if let Some(id) = data.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(u) = data.get("usage").filter(|u| !u.is_null()) {
            self.usage = parse_usage(u);
        }
        let mut out = Vec::new();
        if let Some(choice) = data.pointer("/choices/0") {
            if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str)
                && !text.is_empty()
            {
                out.push(ProviderEvent::Delta {
                    kind: DeltaKind::Text,
                    text: text.to_owned(),
                });
            }
            if let Some(calls) = choice.pointer("/delta/tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let index = c.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let args = c.pointer("/function/arguments").and_then(Value::as_str).unwrap_or_default();
                    if let Some(existing) = self.calls.iter_mut().find(|x| x.0 == index) {
                        existing.3.push_str(args);
                        continue;
                    }
                    let call_id = c.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                    let name = c.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_owned();
                    out.push(ProviderEvent::ToolStart {
                        name: "function_call".into(),
                        details: json!({ "index": index, "call_id": call_id, "name": name }),
                    });
                    self.calls.push((index, call_id, name, args.to_owned()));
                }
            }
            if let Some(r) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(r.to_owned());
            }
        }
        out
    }

    fn parse_completion(&self, body: &Value) -> Result<CompletionResult, String> {
        if body.get("error").is_some_and(|e| !e.is_null()) {
            return Err(parse_error_payload(&body.to_string()).1);
        }
        Ok(CompletionResult {
            text: body
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            usage: body.get("usage").and_then(parse_usage),
        })
    }
}
