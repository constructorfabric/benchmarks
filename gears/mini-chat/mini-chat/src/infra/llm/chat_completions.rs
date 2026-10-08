//! Chat Completions API adapter. Built-in tools (`file_search`,
//! `web_search`, `code_interpreter`) are dropped; function tools are kept.

use std::collections::{BTreeMap, VecDeque};

use futures::StreamExt;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;

use super::{
    ChatRequest, Completion, LlmEvent, LlmEventStream, LlmGateway, ProviderError,
    ProviderErrorKind, ResolvedProvider, chat_url, error_from_response, insert_sampling, json_post,
    merge_extra_body, parse_usage, stream_read_error,
};

fn messages(req: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::new();
    if !req.instructions.is_empty() {
        out.push(json!({"role": "system", "content": req.instructions}));
    }
    for m in &req.messages {
        out.push(json!({"role": m.role, "content": m.text}));
    }
    for item in &req.extra_input {
        // Agentic loop items are translated to chat messages.
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => out.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                    "type": "function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(Value::Null),
                        "arguments": item.get("arguments").cloned().unwrap_or(json!("{}")),
                    }
                }]
            })),
            Some("function_call_output") => out.push(json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": item.get("output").cloned().unwrap_or(json!("")),
            })),
            _ => {}
        }
    }
    out
}

#[must_use]
pub fn build_body(req: &ChatRequest) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages(req)));
    body.insert("stream".into(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
    body.insert("user".into(), json!(req.user));
    if req.tools.knowledge_search {
        body.insert(
            "tools".into(),
            json!([{
                "type": "function",
                "function": {
                    "name": "search_knowledge",
                    "description": "Search the organization knowledge base and return relevant excerpts.",
                    "parameters": {
                        "type": "object",
                        "properties": {"query": {"type": "string"}, "top_k": {"type": "integer"}},
                        "required": ["query"]
                    }
                }
            }]),
        );
    }
    insert_sampling(&mut body, &req.api_params);
    if !req.api_params.stop.is_empty() {
        body.insert("stop".into(), json!(req.api_params.stop));
    }
    if let Some(effort) = &req.api_params.reasoning_effort {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    merge_extra_body(&mut body, &req.api_params);
    Value::Object(body)
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
pub struct ChatParser {
    finish: Option<String>,
    usage: Option<mini_chat_sdk::UsageTokens>,
    response_id: Option<String>,
    tools: BTreeMap<u64, ToolAcc>,
    started: Vec<u64>,
}

impl ChatParser {
    pub fn on_event(&mut self, ev: &ServerEvent) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        if ev.data.trim() == "[DONE]" {
            out.extend(self.finish_events());
            return out;
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return out;
        };
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error");
            out.push(LlmEvent::Failed(ProviderError::new(
                ProviderErrorKind::ProviderError,
                msg,
            )));
            return out;
        }
        if self.response_id.is_none() {
            self.response_id = v.get("id").and_then(Value::as_str).map(str::to_owned);
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = parse_usage(u);
        }
        if let Some(choice) = v
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            let delta = choice.get("delta").cloned().unwrap_or(Value::Null);
            if let Some(t) = delta.get("content").and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(LlmEvent::TextDelta(t.to_owned()));
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let idx = c.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let acc = self.tools.entry(idx).or_default();
                    if let Some(id) = c.get("id").and_then(Value::as_str) {
                        id.clone_into(&mut acc.id);
                    }
                    if let Some(n) = c.pointer("/function/name").and_then(Value::as_str) {
                        acc.name.push_str(n);
                    }
                    if let Some(a) = c.pointer("/function/arguments").and_then(Value::as_str) {
                        acc.args.push_str(a);
                    }
                    if !self.started.contains(&idx) {
                        self.started.push(idx);
                        out.push(LlmEvent::ToolStart {
                            name: "function_call".into(),
                            details: json!({"index": idx, "call_id": acc.id, "name": acc.name}),
                        });
                    }
                }
            }
            if let Some(f) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish = Some(f.to_owned());
            }
        }
        out
    }

    fn finish_events(&mut self) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        if !self.tools.is_empty() {
            for acc in std::mem::take(&mut self.tools).into_values() {
                out.push(LlmEvent::ToolDone {
                    name: "function_call".into(),
                    details: json!({"call_id": acc.id, "name": acc.name, "arguments": acc.args}),
                });
                out.push(LlmEvent::FunctionCall {
                    call_id: acc.id,
                    name: acc.name,
                    arguments: acc.args,
                });
            }
            return out;
        }
        match self.finish.as_deref() {
            Some("length") => out.push(LlmEvent::Incomplete {
                usage: self.usage,
                response_id: self.response_id.clone(),
                reason: "max_tokens".into(),
            }),
            Some("content_filter") => out.push(LlmEvent::Incomplete {
                usage: self.usage,
                response_id: self.response_id.clone(),
                reason: "content_filter".into(),
            }),
            _ => out.push(LlmEvent::Completed {
                usage: self.usage,
                response_id: self.response_id.clone(),
                citations: vec![],
            }),
        }
        out
    }
}

struct StreamState {
    events: ServerEventsStream<ServerEvent>,
    parser: ChatParser,
    pending: VecDeque<LlmEvent>,
    done: bool,
}

/// # Errors
/// Provider errors before the stream starts.
pub async fn stream(
    gw: &LlmGateway,
    ctx: &SecurityContext,
    provider: &ResolvedProvider,
    req: &ChatRequest,
) -> Result<LlmEventStream, ProviderError> {
    let body = build_body(req);
    let resp = gw
        .proxy(ctx, json_post(&chat_url(provider, &req.model), &body)?)
        .await?;
    if !resp.status().is_success() {
        return Err(error_from_response(resp).await);
    }
    match ServerEventsStream::from_response::<ServerEvent>(resp) {
        ServerEventsResponse::Events(events) => {
            let st = StreamState {
                events,
                parser: ChatParser::default(),
                pending: VecDeque::new(),
                done: false,
            };
            Ok(Box::pin(futures::stream::unfold(st, |mut st| async move {
                loop {
                    if let Some(e) = st.pending.pop_front() {
                        if e.is_terminal() {
                            st.done = true;
                            st.pending.clear();
                        } else if matches!(e, LlmEvent::FunctionCall { .. })
                            && st.pending.is_empty()
                        {
                            st.done = true;
                        }
                        return Some((e, st));
                    }
                    if st.done {
                        return None;
                    }
                    match st.events.next().await {
                        Some(Ok(ev)) => {
                            let evs = st.parser.on_event(&ev);
                            st.pending.extend(evs);
                        }
                        Some(Err(e)) => {
                            st.done = true;
                            return Some((LlmEvent::Failed(stream_read_error(&e.to_string())), st));
                        }
                        None => {
                            let evs = st.parser.finish_events();
                            st.pending.extend(evs);
                            if st.pending.is_empty() {
                                return None;
                            }
                        }
                    }
                }
            })))
        }
        ServerEventsResponse::Response(resp) => {
            let c = parse_json_completion(resp).await?;
            let mut evs = Vec::new();
            if !c.text.is_empty() {
                evs.push(LlmEvent::TextDelta(c.text));
            }
            evs.push(LlmEvent::Completed {
                usage: c.usage,
                response_id: None,
                citations: vec![],
            });
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }
}

async fn parse_json_completion(
    resp: http::Response<oagw_sdk::body::Body>,
) -> Result<Completion, ProviderError> {
    let bytes = resp
        .into_body()
        .into_bytes()
        .await
        .map_err(|e| stream_read_error(&e.to_string()))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::ProviderError,
            "Invalid provider response",
        )
    })?;
    let text = v
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    Ok(Completion {
        text,
        usage: v.get("usage").and_then(parse_usage),
    })
}

/// # Errors
/// Provider errors.
pub async fn complete(
    gw: &LlmGateway,
    ctx: &SecurityContext,
    provider: &ResolvedProvider,
    req: &ChatRequest,
) -> Result<Completion, ProviderError> {
    let mut r = req.clone();
    r.stream = false;
    let resp = gw
        .proxy(
            ctx,
            json_post(&chat_url(provider, &r.model), &build_body(&r))?,
        )
        .await?;
    if !resp.status().is_success() {
        return Err(error_from_response(resp).await);
    }
    parse_json_completion(resp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(data: &str) -> ServerEvent {
        ServerEvent {
            id: None,
            event: None,
            data: data.to_owned(),
            retry: None,
        }
    }

    #[test]
    fn parses_chunks_and_usage() {
        let mut p = ChatParser::default();
        let d = p.on_event(&ev(
            r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"Hi"}}]}"#,
        ));
        assert_eq!(d, vec![LlmEvent::TextDelta("Hi".into())]);
        p.on_event(&ev(
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        ));
        p.on_event(&ev(
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2}}"#,
        ));
        let d = p.on_event(&ev("[DONE]"));
        assert!(
            matches!(&d[0], LlmEvent::Incomplete { reason, usage, .. } if reason == "max_tokens" && usage.unwrap().output_tokens == 2)
        );
    }
}
