//! `OpenAI` / Azure `OpenAI` Responses API adapter (also used, without tools and
//! metadata, for the vLLM Responses API).

use std::collections::{HashMap, VecDeque};

use futures::StreamExt;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;

use super::{
    ChatRequest, CitationSource, Completion, LlmEvent, LlmEventStream, LlmGateway, ProviderError,
    ProviderErrorKind, RawCitation, ResolvedProvider, chat_url, error_from_response,
    insert_sampling, json_post, merge_extra_body, parse_usage, stream_read_error,
};
use crate::config::ProviderKind;

const CI_OUTPUT_CAP: usize = 8192;

fn input_items(req: &ChatRequest) -> Vec<Value> {
    let mut items: Vec<Value> = req
        .messages
        .iter()
        .map(|m| {
            if m.image_file_ids.is_empty() {
                json!({"role": m.role, "content": m.text})
            } else {
                let mut content = vec![json!({"type": "input_text", "text": m.text})];
                for f in &m.image_file_ids {
                    content.push(json!({"type": "input_image", "file_id": f}));
                }
                json!({"role": m.role, "content": content})
            }
        })
        .collect();
    items.extend(req.extra_input.iter().cloned());
    items
}

/// `search_knowledge` function tool definition.
#[must_use]
pub fn knowledge_tool_responses() -> Value {
    json!({
        "type": "function",
        "name": "search_knowledge",
        "description": "Search the organization knowledge base and return relevant excerpts.",
        "parameters": {
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "top_k": {"type": "integer", "description": "Maximum number of results"}
            },
            "required": ["query"]
        }
    })
}

/// Build the Responses API request body.
#[must_use]
pub fn build_body(req: &ChatRequest, kind: ProviderKind) -> Value {
    let vllm = kind == ProviderKind::VllmResponses;
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), json!(req.instructions));
    }
    body.insert("input".into(), Value::Array(input_items(req)));
    body.insert("stream".into(), json!(req.stream));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    body.insert("store".into(), json!(false));
    body.insert("user".into(), json!(req.user));
    if !vllm {
        body.insert(
            "metadata".into(),
            json!({
                "tenant_id": req.metadata.tenant_id,
                "user_id": req.metadata.user_id,
                "chat_id": req.metadata.chat_id,
                "request_type": req.metadata.request_type,
                "feature": req.metadata.feature,
            }),
        );
        let mut tools = Vec::new();
        if let Some((vs, n)) = &req.tools.file_search {
            tools.push(
                json!({"type": "file_search", "vector_store_ids": [vs], "max_num_results": n}),
            );
        }
        if let Some(size) = &req.tools.web_search {
            tools.push(json!({"type": "web_search", "search_context_size": size}));
        }
        if let Some(files) = &req.tools.code_interpreter {
            tools.push(json!({"type": "code_interpreter", "container": {"type": "auto", "file_ids": files}}));
            body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
        }
        if req.tools.knowledge_search {
            tools.push(knowledge_tool_responses());
        }
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
            body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
        }
    }
    insert_sampling(&mut body, &req.api_params);
    if !req.api_params.stop.is_empty() {
        body.insert("stop".into(), json!(req.api_params.stop));
    }
    if let Some(effort) = &req.api_params.reasoning_effort {
        body.insert("reasoning".into(), json!({"effort": effort}));
    }
    merge_extra_body(&mut body, &req.api_params);
    Value::Object(body)
}

fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(cap).collect();
        out.push_str("...[truncated]");
        out
    }
}

fn char_slice(text: &str, start: u64, end: u64) -> String {
    let (Ok(s), Ok(e)) = (usize::try_from(start), usize::try_from(end)) else {
        return String::new();
    };
    let len = text.chars().count();
    if s >= e || e > len {
        return String::new();
    }
    text.chars().skip(s).take(e - s).collect()
}

/// Map one provider annotation to a raw citation.
#[must_use]
pub fn annotation_to_citation(a: &Value, part_text: &str) -> Option<RawCitation> {
    let ty = a.get("type").and_then(Value::as_str).unwrap_or("");
    let start = a.get("start_index").and_then(Value::as_u64);
    let end = a.get("end_index").and_then(Value::as_u64);
    let span = match (start, end) {
        (Some(s), Some(e)) => Some((s, e)),
        _ => None,
    };
    match ty {
        "url_citation" => {
            let url = a.get("url").and_then(Value::as_str)?.to_owned();
            let title = a
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let snippet = match a.get("text").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => t.to_owned(),
                _ => span
                    .map(|(s, e)| char_slice(part_text, s, e))
                    .unwrap_or_default(),
            };
            Some(RawCitation {
                source: CitationSource::Web { url, title },
                snippet,
                span,
            })
        }
        "file_citation" | "container_file_citation" | "file_path" => {
            let file_id = a.get("file_id").and_then(Value::as_str)?.to_owned();
            Some(RawCitation {
                source: CitationSource::File {
                    file_id,
                    filename: a.get("filename").and_then(Value::as_str).map(str::to_owned),
                },
                snippet: String::new(),
                span,
            })
        }
        _ => None,
    }
}

/// Citations of a final response object.
#[must_use]
pub fn citations_from_response(resp: &Value) -> Vec<RawCitation> {
    let mut out = Vec::new();
    if let Some(items) = resp.get("output").and_then(Value::as_array) {
        for item in items {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                    if let Some(anns) = part.get("annotations").and_then(Value::as_array) {
                        for a in anns {
                            if let Some(c) = annotation_to_citation(a, text) {
                                out.push(c);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Concatenated `output_text` of a final response object.
#[must_use]
pub fn output_text(resp: &Value) -> String {
    if let Some(s) = resp.get("output_text").and_then(Value::as_str) {
        return s.to_owned();
    }
    let mut text = String::new();
    if let Some(items) = resp.get("output").and_then(Value::as_array) {
        for item in items {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                }
            }
        }
    }
    text
}

fn error_from_value(v: &Value, fallback_raw: &str) -> ProviderError {
    let err = v
        .pointer("/response/error")
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error").filter(|e| !e.is_null()));
    let (code, msg) = match err {
        Some(e) if e.is_object() => (
            e.get("code").and_then(Value::as_str).map(str::to_owned),
            e.get("message")
                .and_then(Value::as_str)
                .unwrap_or("provider error")
                .to_owned(),
        ),
        Some(e) if e.is_string() => (None, e.as_str().unwrap_or("").to_owned()),
        _ => match (
            v.get("code").and_then(Value::as_str),
            v.get("message").and_then(Value::as_str),
        ) {
            (c, Some(m)) => (c.map(str::to_owned), m.to_owned()),
            _ => (None, fallback_raw.to_owned()),
        },
    };
    let mut e = ProviderError::new(ProviderErrorKind::ProviderError, &msg);
    e.provider_code = code;
    e.usage = v.pointer("/response/usage").and_then(parse_usage);
    e
}

/// Incremental parser state of one Responses stream.
#[derive(Debug, Default)]
pub struct ResponsesParser {
    pub vllm: bool,
    part_text: HashMap<(u64, u64), String>,
    added_citations: Vec<RawCitation>,
    in_think: bool,
    pending: String,
}

impl ResponsesParser {
    #[must_use]
    pub fn new(vllm: bool) -> Self {
        Self {
            vllm,
            ..Self::default()
        }
    }

    fn split_think(&mut self, delta: &str, out: &mut Vec<LlmEvent>) {
        // vLLM: text inside <think> ... </think> is reasoning.
        self.pending.push_str(delta);
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(pos) = self.pending.find(tag) {
                let before: String = self.pending[..pos].to_owned();
                if !before.is_empty() {
                    out.push(if self.in_think {
                        LlmEvent::ReasoningDelta(before)
                    } else {
                        LlmEvent::TextDelta(before)
                    });
                }
                self.pending = self.pending[pos + tag.len()..].to_owned();
                self.in_think = !self.in_think;
                continue;
            }
            // keep a possible partial tag at the end
            let keep = (1..tag.len())
                .rev()
                .find(|n| self.pending.ends_with(&tag[..*n]))
                .unwrap_or(0);
            let emit_len = self.pending.len() - keep;
            if emit_len > 0 {
                let text: String = self.pending[..emit_len].to_owned();
                out.push(if self.in_think {
                    LlmEvent::ReasoningDelta(text)
                } else {
                    LlmEvent::TextDelta(text)
                });
                self.pending = self.pending[emit_len..].to_owned();
            }
            break;
        }
    }

    /// Translate one provider SSE event.
    #[allow(clippy::similar_names, reason = "conventional names (cond/res/rest)")]
    pub fn on_event(&mut self, ev: &ServerEvent) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        if ev.data.trim() == "[DONE]" {
            return out;
        }
        let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
        let name = match ev.event.as_deref() {
            Some(n) if !n.is_empty() && n != "message" => n.to_owned(),
            _ => data
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        };
        match name.as_str() {
            "response.output_text.delta" => {
                let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
                let key = (
                    data.get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    data.get("content_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
                self.part_text.entry(key).or_default().push_str(delta);
                if !delta.is_empty() {
                    if self.vllm {
                        self.split_think(delta, &mut out);
                    } else {
                        out.push(LlmEvent::TextDelta(delta.to_owned()));
                    }
                }
            }
            "response.reasoning_text.delta" if self.vllm => {
                if let Some(d) = data.get("delta").and_then(Value::as_str) {
                    out.push(LlmEvent::ReasoningDelta(d.to_owned()));
                }
            }
            "response.output_text.annotation.added" => {
                let key = (
                    data.get("output_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    data.get("content_index")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                );
                let text = self.part_text.get(&key).cloned().unwrap_or_default();
                if let Some(a) = data.get("annotation")
                    && let Some(c) = annotation_to_citation(a, &text)
                {
                    self.added_citations.push(c);
                }
            }
            "response.file_search_call.searching" => out.push(LlmEvent::ToolStart {
                name: "file_search".into(),
                details: json!({}),
            }),
            "response.file_search_call.completed" => {
                let n = data
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                out.push(LlmEvent::ToolDone {
                    name: "file_search".into(),
                    details: json!({"files_searched": n}),
                });
            }
            "response.web_search_call.searching" => out.push(LlmEvent::ToolStart {
                name: "web_search".into(),
                details: json!({}),
            }),
            "response.web_search_call.completed" => out.push(LlmEvent::ToolDone {
                name: "web_search".into(),
                details: json!({}),
            }),
            "response.code_interpreter_call.in_progress" => out.push(LlmEvent::ToolStart {
                name: "code_interpreter".into(),
                details: json!({}),
            }),
            "response.output_item.done" => {
                let item = data.get("item").cloned().unwrap_or(Value::Null);
                match item.get("type").and_then(Value::as_str) {
                    Some("code_interpreter_call") => {
                        let logs: Vec<String> = item
                            .get("outputs")
                            .and_then(Value::as_array)
                            .map(|outs| {
                                outs.iter()
                                    .filter(|o| {
                                        o.get("type").and_then(Value::as_str) == Some("logs")
                                    })
                                    .filter_map(|o| {
                                        o.get("logs").and_then(Value::as_str).map(str::to_owned)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        out.push(LlmEvent::ToolDone {
                            name: "code_interpreter".into(),
                            details: json!({"output": truncate_chars(&logs.join("\n"), CI_OUTPUT_CAP)}),
                        });
                    }
                    Some("function_call") => out.push(LlmEvent::FunctionCall {
                        call_id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_owned(),
                    }),
                    _ => {}
                }
            }
            "response.completed" => {
                let resp = data.get("response").cloned().unwrap_or(Value::Null);
                if self.vllm && !self.pending.is_empty() {
                    let rest = std::mem::take(&mut self.pending);
                    out.push(if self.in_think {
                        LlmEvent::ReasoningDelta(rest)
                    } else {
                        LlmEvent::TextDelta(rest)
                    });
                }
                let mut citations = citations_from_response(&resp);
                if citations.is_empty() {
                    citations = std::mem::take(&mut self.added_citations);
                }
                out.push(LlmEvent::Completed {
                    usage: resp.get("usage").and_then(parse_usage),
                    response_id: resp.get("id").and_then(Value::as_str).map(str::to_owned),
                    citations,
                });
            }
            "response.incomplete" => {
                let resp = data.get("response").cloned().unwrap_or(Value::Null);
                let reason = resp
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("other")
                    .to_owned();
                out.push(LlmEvent::Incomplete {
                    usage: resp.get("usage").and_then(parse_usage),
                    response_id: resp.get("id").and_then(Value::as_str).map(str::to_owned),
                    reason,
                });
            }
            "response.failed" | "error" => {
                out.push(LlmEvent::Failed(error_from_value(&data, &ev.data)));
            }
            _ => {}
        }
        out
    }
}

/// Convert a non-SSE (JSON) response body into events.
#[must_use]
pub fn events_from_json(v: &Value) -> Vec<LlmEvent> {
    if v.get("error").is_some_and(|e| !e.is_null()) {
        return vec![LlmEvent::Failed(error_from_value(v, "provider error"))];
    }
    let text = output_text(v);
    let mut out = Vec::new();
    if !text.is_empty() {
        out.push(LlmEvent::TextDelta(text));
    }
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    if status == "incomplete" {
        out.push(LlmEvent::Incomplete {
            usage: v.get("usage").and_then(parse_usage),
            response_id: v.get("id").and_then(Value::as_str).map(str::to_owned),
            reason: v
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("other")
                .to_owned(),
        });
    } else {
        out.push(LlmEvent::Completed {
            usage: v.get("usage").and_then(parse_usage),
            response_id: v.get("id").and_then(Value::as_str).map(str::to_owned),
            citations: citations_from_response(v),
        });
    }
    out
}

struct StreamState {
    events: ServerEventsStream<ServerEvent>,
    parser: ResponsesParser,
    pending: VecDeque<LlmEvent>,
    done: bool,
}

/// Start a streaming Responses request.
///
/// # Errors
/// Provider errors before the stream starts.
pub async fn stream(
    gw: &LlmGateway,
    ctx: &SecurityContext,
    provider: &ResolvedProvider,
    req: &ChatRequest,
) -> Result<LlmEventStream, ProviderError> {
    let body = build_body(req, provider.kind);
    let http_req = json_post(&chat_url(provider, &req.model), &body)?;
    let resp = gw.proxy(ctx, http_req).await?;
    if !resp.status().is_success() {
        return Err(error_from_response(resp).await);
    }
    let vllm = provider.kind == ProviderKind::VllmResponses;
    match ServerEventsStream::from_response::<ServerEvent>(resp) {
        ServerEventsResponse::Events(events) => {
            let state = StreamState {
                events,
                parser: ResponsesParser::new(vllm),
                pending: VecDeque::new(),
                done: false,
            };
            Ok(Box::pin(futures::stream::unfold(
                state,
                |mut st| async move {
                    loop {
                        if let Some(e) = st.pending.pop_front() {
                            if e.is_terminal() {
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
                                return Some((
                                    LlmEvent::Failed(stream_read_error(&e.to_string())),
                                    st,
                                ));
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
                },
            )))
        }
        ServerEventsResponse::Response(resp) => {
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
            Ok(Box::pin(futures::stream::iter(events_from_json(&v))))
        }
    }
}

/// Non-streaming Responses request.
///
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
    let body = build_body(&r, provider.kind);
    let http_req = json_post(&chat_url(provider, &r.model), &body)?;
    let resp = gw.proxy(ctx, http_req).await?;
    if !resp.status().is_success() {
        return Err(error_from_response(resp).await);
    }
    match ServerEventsStream::from_response::<ServerEvent>(resp) {
        ServerEventsResponse::Events(mut events) => {
            // Some servers answer with SSE even when stream=false.
            let mut parser = ResponsesParser::new(provider.kind == ProviderKind::VllmResponses);
            let mut text = String::new();
            while let Some(ev) = events.next().await {
                let ev = ev.map_err(|e| stream_read_error(&e.to_string()))?;
                for e in parser.on_event(&ev) {
                    match e {
                        LlmEvent::TextDelta(t) => text.push_str(&t),
                        LlmEvent::Completed { usage, .. } | LlmEvent::Incomplete { usage, .. } => {
                            return Ok(Completion { text, usage });
                        }
                        LlmEvent::Failed(err) => return Err(err),
                        _ => {}
                    }
                }
            }
            Err(ProviderError::new(
                ProviderErrorKind::ProviderError,
                "Provider stream ended unexpectedly",
            ))
        }
        ServerEventsResponse::Response(resp) => {
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
            if v.get("error").is_some_and(|e| !e.is_null()) {
                return Err(error_from_value(&v, "provider error"));
            }
            Ok(Completion {
                text: output_text(&v),
                usage: v.get("usage").and_then(parse_usage),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::llm::{ChatMessage, RequestMetadata, ToolsSpec};

    #[allow(clippy::needless_pass_by_value, reason = "test helper")]
    fn ev(name: Option<&str>, data: Value) -> ServerEvent {
        ServerEvent {
            id: None,
            event: name.map(str::to_owned),
            data: data.to_string(),
            retry: None,
        }
    }

    fn req() -> ChatRequest {
        ChatRequest {
            model: "gpt".into(),
            instructions: "be nice".into(),
            messages: vec![ChatMessage {
                role: "user",
                text: "hi".into(),
                image_file_ids: vec!["file-1".into()],
            }],
            max_output_tokens: 100,
            tools: ToolsSpec {
                file_search: Some(("vs_1".into(), 5)),
                web_search: Some("low".into()),
                code_interpreter: Some(vec!["file-x".into()]),
                knowledge_search: false,
                web_search_max_uses: 2,
            },
            max_tool_calls: 2,
            api_params: mini_chat_sdk::ModelApiParams {
                temperature: Some(0.5),
                ..Default::default()
            },
            user: "u".into(),
            metadata: RequestMetadata {
                tenant_id: "t".into(),
                user_id: "u".into(),
                chat_id: "c".into(),
                request_type: "chat",
                feature: "file_search+web_search+code_interpreter".into(),
            },
            stream: true,
            extra_input: vec![],
        }
    }

    #[test]
    fn body_shape() {
        let b = build_body(&req(), ProviderKind::OpenaiResponses);
        assert_eq!(b["model"], "gpt");
        assert_eq!(b["instructions"], "be nice");
        assert_eq!(b["stream"], true);
        assert_eq!(b["max_output_tokens"], 100);
        assert_eq!(b["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(b["tools"][0]["type"], "file_search");
        assert_eq!(b["tools"][0]["vector_store_ids"][0], "vs_1");
        assert_eq!(b["tools"][1]["type"], "web_search");
        assert_eq!(b["tools"][2]["container"]["file_ids"][0], "file-x");
        assert_eq!(b["include"][0], "code_interpreter_call.outputs");
        assert_eq!(b["max_tool_calls"], 2);
        assert_eq!(b["metadata"]["request_type"], "chat");
        assert_eq!(b["temperature"], 0.5);
        assert!(b.get("top_p").is_none());
        let v = build_body(&req(), ProviderKind::VllmResponses);
        assert!(v.get("tools").is_none());
        assert!(v.get("metadata").is_none());
    }

    #[test]
    fn translates_events() {
        let mut p = ResponsesParser::new(false);
        let d = p.on_event(&ev(
            Some("response.output_text.delta"),
            json!({"delta": "Hel"}),
        ));
        assert_eq!(d, vec![LlmEvent::TextDelta("Hel".into())]);
        // event name from data.type when the event line is missing
        let d = p.on_event(&ev(
            None,
            json!({"type": "response.output_text.delta", "delta": "lo"}),
        ));
        assert_eq!(d, vec![LlmEvent::TextDelta("lo".into())]);
        let d = p.on_event(&ev(Some("response.file_search_call.completed"), json!({})));
        assert_eq!(
            d,
            vec![LlmEvent::ToolDone {
                name: "file_search".into(),
                details: json!({"files_searched": 0})
            }]
        );
        let d = p.on_event(&ev(
            Some("response.output_item.done"),
            json!({"item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "a"}, {"type": "logs", "logs": "b"}]}}),
        ));
        assert_eq!(
            d,
            vec![LlmEvent::ToolDone {
                name: "code_interpreter".into(),
                details: json!({"output": "a\nb"})
            }]
        );
        let d = p.on_event(&ev(
            Some("response.completed"),
            json!({"response": {"id": "resp_1", "usage": {"input_tokens": 3, "output_tokens": 4},
                "output": [{"type": "message", "content": [{"type": "output_text", "text": "Hello world",
                    "annotations": [{"type": "url_citation", "url": "https://x", "title": "X", "start_index": 6, "end_index": 11}]}]}]}}),
        ));
        match &d[0] {
            LlmEvent::Completed {
                usage,
                response_id,
                citations,
            } => {
                assert_eq!(usage.unwrap().output_tokens, 4);
                assert_eq!(response_id.as_deref(), Some("resp_1"));
                assert_eq!(citations[0].snippet, "world");
                assert_eq!(citations[0].span, Some((6, 11)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn failed_and_error_events() {
        let mut p = ResponsesParser::new(false);
        let d = p.on_event(&ev(
            Some("response.failed"),
            json!({"response": {"error": {"code": "server_error", "message": "boom resp_abc"}, "usage": {"input_tokens": 5, "output_tokens": 0}}}),
        ));
        match &d[0] {
            LlmEvent::Failed(e) => {
                assert_eq!(e.kind, ProviderErrorKind::ProviderError);
                assert_eq!(e.message, "boom [provider_id]");
                assert_eq!(e.usage.unwrap().input_tokens, 5);
            }
            other => panic!("unexpected {other:?}"),
        }
        let d = p.on_event(&ev(Some("error"), json!({"code": "x", "message": "bad"})));
        assert!(matches!(&d[0], LlmEvent::Failed(e) if e.message == "bad"));
    }

    #[test]
    fn vllm_think_split() {
        let mut p = ResponsesParser::new(true);
        let mut all = Vec::new();
        for chunk in ["<thi", "nk>reason</th", "ink>answer"] {
            all.extend(p.on_event(&ev(
                Some("response.output_text.delta"),
                json!({"delta": chunk}),
            )));
        }
        assert!(all.contains(&LlmEvent::ReasoningDelta("reason".into())));
        assert!(all.contains(&LlmEvent::TextDelta("answer".into())));
    }

    #[test]
    fn file_citation_has_no_span() {
        let c = annotation_to_citation(
            &json!({"type": "file_citation", "file_id": "file-abc", "filename": "a.pdf", "index": 3}),
            "",
        )
        .unwrap();
        assert!(c.span.is_none());
        assert_eq!(c.snippet, "");
    }
}
