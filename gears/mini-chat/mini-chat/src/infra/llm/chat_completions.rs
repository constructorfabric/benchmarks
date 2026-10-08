//! OpenAI Chat Completions adapter. Built-in tools are dropped (ADR-0005).

use serde_json::{Map, Value, json};

use super::{ContentPart, LlmRequest, ProviderEvent, TranslateState, apply_api_params, codes, parse_usage};

#[must_use]
pub fn build_body(req: &LlmRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({ "role": "system", "content": req.instructions }));
    }
    for m in &req.input {
        let text: Vec<&str> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.as_str()),
                ContentPart::Image(_) => None,
            })
            .collect();
        messages.push(json!({ "role": m.role, "content": text.join("\n") }));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.provider_model_id));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    body.insert("user".into(), json!(req.user));
    apply_api_params(&mut body, &req.api_params, true);
    Value::Object(body)
}

fn completed(state: &mut TranslateState, usage: Option<mini_chat_sdk::UsageTokens>) -> ProviderEvent {
    state.done_emitted = true;
    let incomplete_reason = match state.anthropic_stop.as_deref() {
        Some("length") => Some("max_tokens".to_owned()),
        Some("content_filter") => Some("content_filter".to_owned()),
        _ => None,
    };
    ProviderEvent::Completed { usage, response_id: None, incomplete_reason }
}

pub fn translate(state: &mut TranslateState, data: &str) -> Vec<ProviderEvent> {
    let data = data.trim();
    if state.done_emitted {
        return Vec::new();
    }
    if data == "[DONE]" {
        return vec![completed(state, None)];
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return Vec::new();
    };
    if let Some(err) = v.get("error") {
        let message = err.get("message").and_then(Value::as_str).unwrap_or("Provider returned an error");
        return vec![ProviderEvent::Failed { code: codes::PROVIDER_ERROR, message: message.to_owned(), usage: None }];
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
                if let Some(name) = c.get("function").and_then(|f| f.get("name")).and_then(Value::as_str) {
                    out.push(ProviderEvent::ToolStart {
                        name: "function_call".into(),
                        details: json!({ "index": c.get("index"), "call_id": c.get("id"), "name": name }),
                    });
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            state.anthropic_stop = Some(reason.to_owned());
        }
    }
    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
        out.push(completed(state, parse_usage(u)));
    }
    out
}

#[must_use]
pub fn parse_completion(body: &Value) -> (String, Option<mini_chat_sdk::UsageTokens>) {
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
