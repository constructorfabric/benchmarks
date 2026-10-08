//! Anthropic Messages API adapter. `file_search` is dropped; `web_search`
//! and `code_interpreter` map to Anthropic server tools.

use std::collections::{HashMap, VecDeque};

use futures::StreamExt;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;

use super::{
    ChatRequest, Completion, LlmEvent, LlmEventStream, LlmGateway, ProviderError,
    ProviderErrorKind, ResolvedProvider, chat_url, error_from_response, json_post, parse_usage,
    stream_read_error,
};

fn messages(req: &ChatRequest) -> Vec<Value> {
    let mut out: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            if m.image_file_ids.is_empty() || m.role != "user" {
                json!({"role": m.role, "content": m.text})
            } else {
                let mut content = vec![json!({"type": "text", "text": m.text})];
                for f in &m.image_file_ids {
                    content
                        .push(json!({"type": "image", "source": {"type": "file", "file_id": f}}));
                }
                json!({"role": "user", "content": content})
            }
        })
        .collect();
    for item in &req.extra_input {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let input: Value = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_else(|| json!({}));
                out.push(json!({"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": item.get("call_id").cloned().unwrap_or(Value::Null),
                    "name": item.get("name").cloned().unwrap_or(Value::Null),
                    "input": input,
                }]}));
            }
            Some("function_call_output") => out.push(json!({"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": item.get("call_id").cloned().unwrap_or(Value::Null),
                "content": item.get("output").cloned().unwrap_or(json!("")),
            }]})),
            _ => {}
        }
    }
    out
}

#[must_use]
pub fn build_body(req: &ChatRequest) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("system".into(), json!(req.instructions));
    }
    body.insert("messages".into(), Value::Array(messages(req)));
    body.insert("max_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(req.stream));
    body.insert("metadata".into(), json!({"user_id": req.user}));
    let mut tools = Vec::new();
    if req.tools.web_search.is_some() {
        tools.push(json!({"type": "web_search_20250305", "name": "web_search", "max_uses": req.tools.web_search_max_uses.max(1)}));
    }
    if req.tools.code_interpreter.is_some() {
        tools.push(json!({"type": "code_execution_20250522", "name": "code_execution"}));
    }
    if req.tools.knowledge_search {
        tools.push(json!({
            "name": "search_knowledge",
            "description": "Search the organization knowledge base and return relevant excerpts.",
            "input_schema": {"type": "object", "properties": {"query": {"type": "string"}, "top_k": {"type": "integer"}}, "required": ["query"]}
        }));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    for (k, v) in [
        ("temperature", req.api_params.temperature),
        ("top_p", req.api_params.top_p),
    ] {
        if let Some(v) = v {
            body.insert(k.into(), json!(v));
        }
    }
    if !req.api_params.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(req.api_params.stop));
    }
    Value::Object(body)
}

#[derive(Default)]
struct Block {
    kind: String,
    name: String,
    id: String,
    input: String,
}

#[derive(Default)]
pub struct AnthropicParser {
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    cache_write: i64,
    stop_reason: Option<String>,
    message_id: Option<String>,
    blocks: HashMap<u64, Block>,
}

impl AnthropicParser {
    #[allow(
        clippy::unnecessary_wraps,
        reason = "uniform optional accessor across adapters"
    )]
    fn usage(&self) -> Option<mini_chat_sdk::UsageTokens> {
        Some(mini_chat_sdk::UsageTokens {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_input_tokens: self.cache_read,
            cache_write_input_tokens: self.cache_write,
            reasoning_tokens: 0,
        })
    }

    fn tool_name(name: &str) -> String {
        match name {
            "code_execution" | "bash_code_execution" | "text_editor_code_execution" => {
                "code_interpreter".into()
            }
            "web_search" => "web_search".into(),
            "search_knowledge" | "load_files" => name.into(),
            _ => "unknown_tool".into(),
        }
    }

    pub fn on_event(&mut self, ev: &ServerEvent) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return out;
        };
        let name = ev
            .event
            .clone()
            .filter(|n| !n.is_empty() && n != "message")
            .or_else(|| v.get("type").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        match name.as_str() {
            "message_start" => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                self.message_id = m.get("id").and_then(Value::as_str).map(str::to_owned);
                if let Some(u) = m.get("usage").and_then(parse_usage) {
                    self.input_tokens = u.input_tokens;
                    self.output_tokens = u.output_tokens;
                    self.cache_read = u.cache_read_input_tokens;
                    self.cache_write = u.cache_write_input_tokens;
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let cb = v.get("content_block").cloned().unwrap_or(Value::Null);
                let kind = cb
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let bname = cb
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if kind == "server_tool_use" || kind == "tool_use" {
                    out.push(LlmEvent::ToolStart {
                        name: Self::tool_name(&bname),
                        details: json!({}),
                    });
                }
                self.blocks.insert(
                    idx,
                    Block {
                        kind,
                        name: bname,
                        id: cb
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        input: String::new(),
                    },
                );
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = d.get("text").and_then(Value::as_str) {
                            out.push(LlmEvent::TextDelta(t.to_owned()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(b), Some(p)) = (
                            self.blocks.get_mut(&idx),
                            d.get("partial_json").and_then(Value::as_str),
                        ) {
                            b.input.push_str(p);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(b) = self.blocks.get(&idx) {
                    if b.kind == "server_tool_use" && Self::tool_name(&b.name) == "code_interpreter"
                    {
                        out.push(LlmEvent::ToolDone {
                            name: "code_interpreter".into(),
                            details: json!({}),
                        });
                    } else if b.kind == "server_tool_use" && b.name == "web_search" {
                        out.push(LlmEvent::ToolDone {
                            name: "web_search".into(),
                            details: json!({}),
                        });
                    }
                }
            }
            "message_delta" => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_owned());
                }
                if let Some(o) = v.pointer("/usage/output_tokens").and_then(Value::as_i64) {
                    self.output_tokens = o;
                }
                if let Some(i) = v.pointer("/usage/input_tokens").and_then(Value::as_i64)
                    && i > 0
                {
                    self.input_tokens = i;
                }
            }
            "message_stop" => {
                if self.stop_reason.as_deref() == Some("tool_use") {
                    let call = self
                        .blocks
                        .values()
                        .find(|b| b.kind == "tool_use")
                        .map(|b| (b.id.clone(), b.name.clone(), b.input.clone()));
                    if let Some((id, n, args)) = call {
                        out.push(LlmEvent::FunctionCall {
                            call_id: id,
                            name: n,
                            arguments: if args.is_empty() { "{}".into() } else { args },
                        });
                        return out;
                    }
                }
                if self.stop_reason.as_deref() == Some("max_tokens") {
                    out.push(LlmEvent::Incomplete {
                        usage: self.usage(),
                        response_id: self.message_id.clone(),
                        reason: "max_tokens".into(),
                    });
                } else {
                    out.push(LlmEvent::Completed {
                        usage: self.usage(),
                        response_id: self.message_id.clone(),
                        citations: vec![],
                    });
                }
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error");
                let kind = if v.pointer("/error/type").and_then(Value::as_str)
                    == Some("rate_limit_error")
                {
                    ProviderErrorKind::RateLimited
                } else {
                    ProviderErrorKind::ProviderError
                };
                out.push(LlmEvent::Failed(ProviderError::new(kind, msg)));
            }
            _ => {}
        }
        out
    }
}

struct StreamState {
    events: ServerEventsStream<ServerEvent>,
    parser: AnthropicParser,
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
    let mut http_req = json_post(&chat_url(provider, &req.model), &build_body(req))?;
    http_req.headers_mut().insert(
        "anthropic-version",
        http::HeaderValue::from_static("2023-06-01"),
    );
    let resp = gw.proxy(ctx, http_req).await?;
    if !resp.status().is_success() {
        return Err(error_from_response(resp).await);
    }
    match ServerEventsStream::from_response::<ServerEvent>(resp) {
        ServerEventsResponse::Events(events) => {
            let st = StreamState {
                events,
                parser: AnthropicParser::default(),
                pending: VecDeque::new(),
                done: false,
            };
            Ok(Box::pin(futures::stream::unfold(st, |mut st| async move {
                loop {
                    if let Some(e) = st.pending.pop_front() {
                        if e.is_terminal() || matches!(e, LlmEvent::FunctionCall { .. }) {
                            st.done = true;
                            st.pending.clear();
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
                            st.done = true;
                            return Some((
                                LlmEvent::Failed(ProviderError::new(
                                    ProviderErrorKind::ProviderError,
                                    "Provider stream ended unexpectedly",
                                )),
                                st,
                            ));
                        }
                    }
                }
            })))
        }
        ServerEventsResponse::Response(_) => Err(ProviderError::new(
            ProviderErrorKind::ProviderError,
            "Invalid provider response",
        )),
    }
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
    let mut http_req = json_post(&chat_url(provider, &r.model), &build_body(&r))?;
    http_req.headers_mut().insert(
        "anthropic-version",
        http::HeaderValue::from_static("2023-06-01"),
    );
    let resp = gw.proxy(ctx, http_req).await?;
    if !resp.status().is_success() {
        return Err(error_from_response(resp).await);
    }
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
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    Ok(Completion {
        text,
        usage: v.get("usage").and_then(parse_usage),
    })
}
