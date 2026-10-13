//! Chat Completions adapter (`openai_chat_completions`): drops built-in
//! tools, keeps function tools.

use serde_json::{Map, Value, json};

use super::openai_responses::apply_extra_body;
use super::{Adapter, ChatItem, Completion, ItemRole, LlmEvent, LlmFailure, LlmRequest, ParseState, Usage, error_message};

pub struct ChatCompletions;

fn usage_of(v: &Value) -> Option<Usage> {
    let u = v.as_object()?;
    let get = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    Some(Usage {
        input_tokens: get("prompt_tokens"),
        output_tokens: get("completion_tokens"),
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

fn messages(req: &LlmRequest) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": req.instructions})];
    for ChatItem { role, text, images } in &req.items {
        let role = if *role == ItemRole::User { "user" } else { "assistant" };
        if images.is_empty() {
            out.push(json!({"role": role, "content": text}));
        } else {
            let mut parts = vec![json!({"type": "text", "text": text})];
            for f in images {
                parts.push(json!({"type": "file", "file": {"file_id": f}}));
            }
            out.push(json!({"role": role, "content": parts}));
        }
    }
    for item in &req.extra_input {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => out.push(json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {"name": item.get("name").cloned().unwrap_or(Value::Null), "arguments": item.get("arguments").cloned().unwrap_or(Value::Null)},
                }],
            })),
            Some("function_call_output") => out.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": item.get("output").cloned().unwrap_or(Value::Null),
            })),
            _ => {}
        }
    }
    out
}

impl Adapter for ChatCompletions {
    fn build_body(&self, req: &LlmRequest) -> Value {
        let mut body = Map::new();
        apply_extra_body(&mut body, req);
        body.insert("model".into(), json!(req.model));
        body.insert("stream".into(), json!(req.stream));
        if req.stream {
            body.insert("stream_options".into(), json!({"include_usage": true}));
        }
        body.insert("messages".into(), Value::Array(messages(req)));
        body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
        let p = &req.api_params;
        if let Some(v) = p.temperature { body.insert("temperature".into(), json!(v)); }
        if let Some(v) = p.top_p { body.insert("top_p".into(), json!(v)); }
        if let Some(v) = p.frequency_penalty { body.insert("frequency_penalty".into(), json!(v)); }
        if let Some(v) = p.presence_penalty { body.insert("presence_penalty".into(), json!(v)); }
        if !p.stop.is_empty() { body.insert("stop".into(), json!(p.stop)); }
        if req.tools.knowledge {
            body.insert("tools".into(), json!([{
                "type": "function",
                "function": {"name": super::KNOWLEDGE_TOOL, "description": super::KNOWLEDGE_DESCRIPTION, "parameters": super::knowledge_parameters()},
            }]));
        }
        body.insert("user".into(), json!(req.user));
        Value::Object(body)
    }

    fn parse_event(&self, state: &mut ParseState, _event: Option<&str>, data: &str) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        if data.trim() == "[DONE]" {
            let incomplete = match state.finish_reason.as_deref() {
                Some("length") => Some("max_tokens".to_owned()),
                Some("content_filter") => Some("content_filter".to_owned()),
                _ => None,
            };
            out.push(LlmEvent::Completed(Completion {
                response_id: state.response_id.clone(),
                usage: state.usage,
                incomplete_reason: incomplete,
            }));
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { return out };
        if v.get("error").is_some_and(|e| !e.is_null()) {
            out.push(LlmEvent::Failed(LlmFailure::provider(error_message(&v).unwrap_or_default())));
            return out;
        }
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            state.response_id = Some(id.to_owned());
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()).and_then(usage_of) {
            state.usage = Some(u);
        }
        if let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|c| c.first()) {
            if let Some(d) = choice.get("delta") {
                if let Some(t) = d.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    state.text.push_str(t);
                    out.push(LlmEvent::TextDelta(t.to_owned()));
                }
                if let Some(calls) = d.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let idx = c.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let entry = state.tool_index.entry(idx).or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = c.get("id").and_then(Value::as_str) { entry.0 = id.to_owned(); }
                        if let Some(f) = c.get("function") {
                            if let Some(n) = f.get("name").and_then(Value::as_str) {
                                entry.1 = n.to_owned();
                                out.push(LlmEvent::ToolStart {
                                    name: "function_call".into(),
                                    details: json!({"index": idx, "call_id": entry.0, "name": n}),
                                });
                            }
                            if let Some(a) = f.get("arguments").and_then(Value::as_str) { entry.2.push_str(a); }
                        }
                    }
                }
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                state.finish_reason = Some(fr.to_owned());
                if fr == "tool_calls" {
                    let mut calls: Vec<_> = state.tool_index.drain().collect();
                    calls.sort_by_key(|(k, _)| *k);
                    for (_, (id, name, args)) in calls {
                        out.push(LlmEvent::ToolDone {
                            name: "function_call".into(),
                            details: json!({"call_id": id, "name": name, "arguments": args}),
                        });
                        out.push(LlmEvent::FunctionCall { name, call_id: id, arguments: args });
                    }
                }
            }
        }
        out
    }

    fn parse_complete(&self, body: &Value) -> Result<(String, Option<Usage>), LlmFailure> {
        if body.get("error").is_some_and(|e| !e.is_null()) {
            return Err(LlmFailure::provider(error_message(body).unwrap_or_default()));
        }
        let text = body
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok((text, body.get("usage").and_then(usage_of)))
    }
}
