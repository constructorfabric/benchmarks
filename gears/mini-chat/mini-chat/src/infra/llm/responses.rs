//! `OpenAI` / Azure `OpenAI` Responses API adapter (`openai_responses`; also `vllm_responses` without
//! tools/metadata). Builds requests and translates the provider SSE stream into internal events.

use std::sync::Arc;

use futures::StreamExt;
use mini_chat_sdk::{ApiParams, UsageTokens, WebSearchContextSize};
use serde_json::{Map, Value, json};
use toolkit_security::SecurityContext;

use super::provider::ResolvedProvider;
use super::sse_parser::{SseFrame, SseParser};
use super::{ProviderRequest, ProviderResponse, ProviderTransport, TransportError};
use crate::config::ProviderKind;
use crate::domain::sanitize::sanitize_provider_message;

/// One message of the provider input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub role: InputRole,
    pub text: String,
    /// Provider file ids of images (user messages only).
    pub image_file_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRole {
    User,
    Assistant,
}

/// Built-in tools sent with a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSet {
    /// `(vector_store_ids, max_num_results)`.
    pub file_search: Option<(Vec<String>, u32)>,
    pub web_search: Option<WebSearchContextSize>,
    /// Container file ids.
    pub code_interpreter: Option<Vec<String>>,
}

impl ToolSet {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.file_search.is_none() && self.web_search.is_none() && self.code_interpreter.is_none()
    }

    /// Metadata `feature` value (`none`, or the tools joined by `+`).
    #[must_use]
    pub fn feature_label(&self) -> String {
        let mut parts = Vec::new();
        if self.file_search.is_some() {
            parts.push("file_search");
        }
        if self.web_search.is_some() {
            parts.push("web_search");
        }
        if self.code_interpreter.is_some() {
            parts.push("code_interpreter");
        }
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join("+")
        }
    }
}

/// A complete chat request (provider-agnostic).
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub provider_model_id: String,
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub tools: ToolSet,
    pub max_output_tokens: u32,
    pub max_tool_calls: u32,
    pub user: String,
    pub metadata: Map<String, Value>,
    pub api_params: ApiParams,
    pub stream: bool,
}

const CONTROLLED_KEYS: &[&str] = &[
    "model",
    "input",
    "messages",
    "instructions",
    "system",
    "stream",
    "stream_options",
    "max_output_tokens",
    "max_completion_tokens",
    "max_tokens",
    "max_tool_calls",
    "tools",
    "tool_choice",
    "include",
    "store",
    "previous_response_id",
    "user",
    "metadata",
];

fn input_item(m: &InputMessage) -> Value {
    match m.role {
        InputRole::User => {
            let mut content = vec![json!({"type": "input_text", "text": m.text})];
            for f in &m.image_file_ids {
                content.push(json!({"type": "input_image", "file_id": f}));
            }
            json!({"role": "user", "content": content})
        }
        InputRole::Assistant => json!({"role": "assistant", "content": m.text}),
    }
}

/// Builds the Responses API request body.
#[must_use]
#[allow(
    clippy::cognitive_complexity,
    reason = "flat field-by-field request body construction gated by provider kind"
)]
pub fn build_responses_body(req: &ChatRequest, kind: ProviderKind) -> Value {
    let with_tools = kind == ProviderKind::OpenaiResponses;
    let mut body = Map::new();
    body.insert("model".into(), json!(req.provider_model_id));
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), json!(req.instructions));
    }
    body.insert(
        "input".into(),
        Value::Array(req.input.iter().map(input_item).collect()),
    );
    body.insert("stream".into(), json!(req.stream));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    if with_tools {
        let mut tools = Vec::new();
        if let Some((vs, n)) = &req.tools.file_search {
            let mut t = json!({"type": "file_search", "vector_store_ids": vs});
            if *n > 0 {
                t["max_num_results"] = json!(n);
            }
            tools.push(t);
        }
        if let Some(size) = req.tools.web_search {
            tools.push(json!({"type": "web_search", "search_context_size": size.as_str()}));
        }
        if let Some(files) = &req.tools.code_interpreter {
            tools.push(json!({"type": "code_interpreter", "container": {"type": "auto", "file_ids": files}}));
            body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
        }
        if !tools.is_empty() {
            body.insert("tools".into(), Value::Array(tools));
        }
        body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
        body.insert("metadata".into(), Value::Object(req.metadata.clone()));
    }
    body.insert("user".into(), json!(req.user));
    let p = &req.api_params;
    if let Some(v) = p.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".into(), json!(v));
    }
    if let Some(v) = p.frequency_penalty {
        body.insert("frequency_penalty".into(), json!(v));
    }
    if let Some(v) = p.presence_penalty {
        body.insert("presence_penalty".into(), json!(v));
    }
    if let Some(e) = &p.reasoning_effort {
        body.insert("reasoning".into(), json!({"effort": e}));
    }
    if let Some(extra) = &p.extra_body {
        for (k, v) in extra {
            if CONTROLLED_KEYS.contains(&k.as_str()) {
                tracing::warn!(key = %k, "mini-chat: ignoring controlled key in extra_body");
            } else {
                body.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(body)
}

/// Builds a Chat Completions request body (`openai_chat_completions`): built-in tools dropped.
#[must_use]
pub fn build_chat_completions_body(req: &ChatRequest) -> Value {
    let mut messages = Vec::new();
    if !req.instructions.is_empty() {
        messages.push(json!({"role": "system", "content": req.instructions}));
    }
    for m in &req.input {
        match m.role {
            InputRole::User => messages.push(json!({"role": "user", "content": m.text})),
            InputRole::Assistant => messages.push(json!({"role": "assistant", "content": m.text})),
        }
    }
    let mut body = json!({
        "model": req.provider_model_id,
        "messages": messages,
        "stream": req.stream,
        "max_completion_tokens": req.max_output_tokens,
        "user": req.user,
    });
    if req.stream {
        body["stream_options"] = json!({"include_usage": true});
    }
    if let Some(v) = req.api_params.temperature {
        body["temperature"] = json!(v);
    }
    body
}

/// Builds an Anthropic Messages request body (`anthropic_messages`).
#[must_use]
pub fn build_anthropic_body(req: &ChatRequest) -> Value {
    let messages: Vec<Value> = req
        .input
        .iter()
        .map(|m| {
            json!({
                "role": match m.role { InputRole::User => "user", InputRole::Assistant => "assistant" },
                "content": m.text,
            })
        })
        .collect();
    let mut body = json!({
        "model": req.provider_model_id,
        "messages": messages,
        "max_tokens": req.max_output_tokens,
        "stream": req.stream,
        "metadata": {"user_id": req.user},
    });
    if !req.instructions.is_empty() {
        body["system"] = json!(req.instructions);
    }
    body
}

/// Raw citation extracted from provider annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    Web {
        url: String,
        title: String,
        snippet: String,
        span: Option<(u64, u64)>,
    },
    File {
        file_id: String,
        filename: String,
    },
}

/// Normalized terminal success.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub usage: Option<UsageTokens>,
    pub response_id: Option<String>,
    pub incomplete_reason: Option<String>,
    pub citations: Vec<RawCitation>,
    /// Final text when the provider sent it only in the terminal response (non-streaming).
    pub output_text: Option<String>,
}

/// Classified provider failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    ProviderError,
    Timeout,
    RateLimited,
}

impl FailKind {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::Timeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Internal stream events produced by the adapters.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    Text(String),
    Reasoning(String),
    ToolStart {
        name: &'static str,
    },
    ToolDone {
        name: &'static str,
        details: Value,
    },
    UnexpectedToolUse(String),
    Completed(Completion),
    Failed {
        kind: FailKind,
        message: String,
        usage: Option<UsageTokens>,
    },
}

fn parse_usage(v: &Value) -> Option<UsageTokens> {
    let u = v.as_object()?;
    let get = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let input = if u.contains_key("input_tokens") {
        get("input_tokens")
    } else {
        get("prompt_tokens")
    };
    let output = if u.contains_key("output_tokens") {
        get("output_tokens")
    } else {
        get("completion_tokens")
    };
    let cached = v
        .pointer("/input_tokens_details/cached_tokens")
        .or_else(|| v.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| u.get("cache_read_input_tokens").and_then(Value::as_i64))
        .unwrap_or(0);
    let reasoning = v
        .pointer("/output_tokens_details/reasoning_tokens")
        .or_else(|| v.pointer("/completion_tokens_details/reasoning_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Some(UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: u
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        reasoning_tokens: reasoning,
    })
}

fn char_slice(text: &str, start: u64, end: u64) -> String {
    let s = usize::try_from(start).unwrap_or(usize::MAX);
    let e = usize::try_from(end).unwrap_or(usize::MAX);
    if s >= e {
        return String::new();
    }
    let n = text.chars().count();
    if e > n {
        return String::new();
    }
    text.chars().skip(s).take(e - s).collect()
}

fn annotation_to_citation(a: &Value, part_text: &str) -> Option<RawCitation> {
    let ty = a.get("type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "url_citation" => {
            let url = a.get("url").and_then(Value::as_str)?.to_owned();
            let title = a
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&url)
                .to_owned();
            let start = a.get("start_index").and_then(Value::as_u64);
            let end = a.get("end_index").and_then(Value::as_u64);
            let span = match (start, end) {
                (Some(s), Some(e)) => Some((s, e)),
                _ => None,
            };
            let snippet = a
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| span.map(|(s, e)| char_slice(part_text, s, e)))
                .unwrap_or_default();
            Some(RawCitation::Web {
                url,
                title,
                snippet,
                span,
            })
        }
        "file_citation" => Some(RawCitation::File {
            file_id: a.get("file_id").and_then(Value::as_str)?.to_owned(),
            filename: a
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        _ => None,
    }
}

/// Extracts citations and the concatenated output text from a terminal `response` object.
fn citations_from_response(resp: &Value) -> (Vec<RawCitation>, Option<String>) {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut any_text = false;
    if let Some(items) = resp.get("output").and_then(Value::as_array) {
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            for part in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if part.get("type").and_then(Value::as_str) != Some("output_text") {
                    continue;
                }
                let part_text = part.get("text").and_then(Value::as_str).unwrap_or("");
                any_text = true;
                text.push_str(part_text);
                for a in part
                    .get("annotations")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some(c) = annotation_to_citation(a, part_text) {
                        out.push(c);
                    }
                }
            }
        }
    }
    (out, any_text.then_some(text))
}

fn error_message(v: &Value) -> String {
    let e = v
        .pointer("/response/error")
        .filter(|e| !e.is_null())
        .or_else(|| v.get("error").filter(|e| !e.is_null()))
        .unwrap_or(v);
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| e.as_str().map(str::to_owned))
        .unwrap_or_else(|| "Provider returned an error".to_owned());
    sanitize_provider_message(&msg)
}

/// Stateful translator of Responses API SSE frames.
#[derive(Debug, Default)]
pub struct ResponsesTranslator {
    text: String,
    annotations: Vec<RawCitation>,
}

impl ResponsesTranslator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Translates one frame into zero or more events.
    pub fn translate(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        let Ok(data) = serde_json::from_str::<Value>(&frame.data) else {
            if frame.event.as_deref() == Some("error") {
                return vec![LlmEvent::Failed {
                    kind: FailKind::ProviderError,
                    message: sanitize_provider_message(&frame.data),
                    usage: None,
                }];
            }
            return Vec::new();
        };
        let name = match frame.event.as_deref() {
            Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
            _ => data
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        };
        match name.as_str() {
            "response.output_text.delta" => {
                let d = data
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                self.text.push_str(&d);
                if d.is_empty() {
                    Vec::new()
                } else {
                    vec![LlmEvent::Text(d)]
                }
            }
            "response.reasoning_text.delta" | "response.reasoning.delta" => {
                let d = data
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if d.is_empty() {
                    Vec::new()
                } else {
                    vec![LlmEvent::Reasoning(d)]
                }
            }
            "response.output_text.annotation.added" => {
                if let Some(a) = data.get("annotation")
                    && let Some(c) = annotation_to_citation(a, &self.text)
                {
                    self.annotations.push(c);
                }
                Vec::new()
            }
            "response.file_search_call.searching" => vec![LlmEvent::ToolStart {
                name: "file_search",
            }],
            "response.file_search_call.completed" => {
                let n = data
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                vec![LlmEvent::ToolDone {
                    name: "file_search",
                    details: json!({"files_searched": n}),
                }]
            }
            "response.web_search_call.searching" => {
                vec![LlmEvent::ToolStart { name: "web_search" }]
            }
            "response.web_search_call.completed" => {
                vec![LlmEvent::ToolDone {
                    name: "web_search",
                    details: json!({}),
                }]
            }
            "response.code_interpreter_call.in_progress" => {
                vec![LlmEvent::ToolStart {
                    name: "code_interpreter",
                }]
            }
            "response.output_item.added" | "response.output_item.done" => {
                let item = data.get("item").cloned().unwrap_or(Value::Null);
                let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
                match (name.as_str(), ty) {
                    ("response.output_item.done", "code_interpreter_call") => {
                        let logs: Vec<String> = item
                            .get("outputs")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
                            .filter_map(|o| {
                                o.get("logs").and_then(Value::as_str).map(str::to_owned)
                            })
                            .collect();
                        let mut output = logs.join("\n");
                        if output.chars().count() > 8192 {
                            output =
                                output.chars().take(8192).collect::<String>() + "...[truncated]";
                        }
                        vec![LlmEvent::ToolDone {
                            name: "code_interpreter",
                            details: json!({"output": output}),
                        }]
                    }
                    ("response.output_item.added", "function_call") => {
                        let n = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned();
                        vec![LlmEvent::UnexpectedToolUse(n)]
                    }
                    _ => Vec::new(),
                }
            }
            "response.completed" | "response.incomplete" => {
                let resp = data.get("response").cloned().unwrap_or(Value::Null);
                let usage = resp.get("usage").and_then(parse_usage);
                let (mut cits, output_text) = citations_from_response(&resp);
                if cits.is_empty() {
                    cits = std::mem::take(&mut self.annotations);
                }
                let incomplete_reason = (name == "response.incomplete").then(|| {
                    resp.pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("other")
                        .to_owned()
                });
                vec![LlmEvent::Completed(Completion {
                    usage,
                    response_id: resp.get("id").and_then(Value::as_str).map(str::to_owned),
                    incomplete_reason,
                    citations: if name == "response.incomplete" {
                        Vec::new()
                    } else {
                        cits
                    },
                    output_text: if self.text.is_empty() {
                        output_text
                    } else {
                        None
                    },
                })]
            }
            "response.failed" => {
                let usage = data.pointer("/response/usage").and_then(parse_usage);
                vec![LlmEvent::Failed {
                    kind: FailKind::ProviderError,
                    message: error_message(&data),
                    usage,
                }]
            }
            "error" => vec![LlmEvent::Failed {
                kind: FailKind::ProviderError,
                message: error_message(&data),
                usage: None,
            }],
            _ => Vec::new(),
        }
    }
}

/// Stateful translator of Chat Completions SSE chunks.
#[derive(Debug, Default)]
pub struct ChatCompletionsTranslator {
    usage: Option<UsageTokens>,
    id: Option<String>,
    finish: Option<String>,
}

impl ChatCompletionsTranslator {
    pub fn translate(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        if frame.data.trim() == "[DONE]" {
            return vec![LlmEvent::Completed(Completion {
                usage: self.usage,
                response_id: self.id.take(),
                incomplete_reason: self
                    .finish
                    .take()
                    .filter(|f| f == "length")
                    .map(|_| "max_tokens".to_owned()),
                citations: Vec::new(),
                output_text: None,
            })];
        }
        let Ok(v) = serde_json::from_str::<Value>(&frame.data) else {
            return Vec::new();
        };
        if v.get("error").is_some() {
            return vec![LlmEvent::Failed {
                kind: FailKind::ProviderError,
                message: error_message(&v),
                usage: None,
            }];
        }
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            self.id = Some(id.to_owned());
        }
        if let Some(u) = v
            .get("usage")
            .filter(|u| !u.is_null())
            .and_then(parse_usage)
        {
            self.usage = Some(u);
        }
        let mut out = Vec::new();
        for c in v
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(t) = c.pointer("/delta/content").and_then(Value::as_str)
                && !t.is_empty()
            {
                out.push(LlmEvent::Text(t.to_owned()));
            }
            if let Some(f) = c.get("finish_reason").and_then(Value::as_str) {
                self.finish = Some(f.to_owned());
            }
        }
        out
    }
}

/// Stateful translator of Anthropic Messages SSE events.
#[derive(Debug, Default)]
pub struct AnthropicTranslator {
    usage: UsageTokens,
    id: Option<String>,
    stop: Option<String>,
}

impl AnthropicTranslator {
    pub fn translate(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        let Ok(v) = serde_json::from_str::<Value>(&frame.data) else {
            return Vec::new();
        };
        let ty = frame.event.clone().unwrap_or_else(|| {
            v.get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        });
        match ty.as_str() {
            "message_start" => {
                self.id = v
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(u) = v.pointer("/message/usage") {
                    self.usage.input_tokens =
                        u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                }
                Vec::new()
            }
            "content_block_delta" => v
                .pointer("/delta/text")
                .and_then(Value::as_str)
                .map(|t| vec![LlmEvent::Text(t.to_owned())])
                .unwrap_or_default(),
            "message_delta" => {
                if let Some(o) = v.pointer("/usage/output_tokens").and_then(Value::as_i64) {
                    self.usage.output_tokens = o;
                }
                self.stop = v
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                Vec::new()
            }
            "message_stop" => vec![LlmEvent::Completed(Completion {
                usage: Some(self.usage),
                response_id: self.id.take(),
                incomplete_reason: self.stop.take().filter(|s| s == "max_tokens"),
                citations: Vec::new(),
                output_text: None,
            })],
            "error" => vec![LlmEvent::Failed {
                kind: FailKind::ProviderError,
                message: error_message(&v),
                usage: None,
            }],
            _ => Vec::new(),
        }
    }
}

enum Translator {
    Responses(ResponsesTranslator),
    Chat(ChatCompletionsTranslator),
    Anthropic(AnthropicTranslator),
}

impl Translator {
    fn new(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
                Self::Responses(ResponsesTranslator::new())
            }
            ProviderKind::OpenaiChatCompletions => Self::Chat(ChatCompletionsTranslator::default()),
            ProviderKind::AnthropicMessages => Self::Anthropic(AnthropicTranslator::default()),
        }
    }

    fn translate(&mut self, f: &SseFrame) -> Vec<LlmEvent> {
        match self {
            Self::Responses(t) => t.translate(f),
            Self::Chat(t) => t.translate(f),
            Self::Anthropic(t) => t.translate(f),
        }
    }
}

/// Builds the provider request for a chat turn.
#[must_use]
pub fn build_provider_request(provider: &ResolvedProvider, req: &ChatRequest) -> ProviderRequest {
    let body = match provider.kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            build_responses_body(req, provider.kind)
        }
        ProviderKind::OpenaiChatCompletions => build_chat_completions_body(req),
        ProviderKind::AnthropicMessages => build_anthropic_body(req),
    };
    let mut r = ProviderRequest::json(
        http::Method::POST,
        &provider.alias,
        provider.chat_path(&req.provider_model_id),
        &body,
    );
    if req.stream {
        r.accept = Some("text/event-stream".to_owned());
    }
    r
}

/// Maps a non-success HTTP response into a failure event.
pub async fn failure_from_response(resp: ProviderResponse) -> LlmEvent {
    let status = resp.status;
    let gateway = resp.gateway;
    let retry_after = resp.retry_after_secs();
    let body = resp.into_bytes().await.unwrap_or_default();
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if status == 429 && !gateway {
        let mut message = "Provider rate limit exceeded".to_owned();
        if let Some(s) = retry_after {
            message = format!("{message}, retry in {s}s");
        }
        return LlmEvent::Failed {
            kind: FailKind::RateLimited,
            message,
            usage: None,
        };
    }
    if gateway && status == 504 {
        return LlmEvent::Failed {
            kind: FailKind::Timeout,
            message: "Provider request timed out".to_owned(),
            usage: None,
        };
    }
    let message = if parsed.is_null() || gateway {
        "Provider is currently unavailable".to_owned()
    } else {
        error_message(&parsed)
    };
    LlmEvent::Failed {
        kind: FailKind::ProviderError,
        message,
        usage: None,
    }
}

/// Maps a transport error into a failure event.
#[must_use]
pub fn failure_from_transport(e: &TransportError) -> LlmEvent {
    match e {
        TransportError::Timeout(_) => LlmEvent::Failed {
            kind: FailKind::Timeout,
            message: "Provider request timed out".to_owned(),
            usage: None,
        },
        _ => LlmEvent::Failed {
            kind: FailKind::ProviderError,
            message: "Provider is currently unavailable".to_owned(),
            usage: None,
        },
    }
}

/// Sends a terminal event; a closed receiver (the turn already ended) is not an error.
async fn forward(tx: &tokio::sync::mpsc::Sender<LlmEvent>, ev: LlmEvent) {
    if tx.send(ev).await.is_err() {
        tracing::debug!("mini-chat: provider event receiver closed");
    }
}

/// Opens a streaming provider call and forwards translated events into `tx` as they arrive.
/// Returns when a terminal event was produced, the stream ended, or `tx` closed.
pub async fn stream_chat(
    transport: Arc<dyn ProviderTransport>,
    ctx: SecurityContext,
    provider: ResolvedProvider,
    req: ChatRequest,
    tx: tokio::sync::mpsc::Sender<LlmEvent>,
) {
    let request = build_provider_request(&provider, &req);
    let resp = match transport.send(ctx, request).await {
        Ok(r) => r,
        Err(e) => {
            forward(&tx, failure_from_transport(&e)).await;
            return;
        }
    };
    if !resp.is_success() {
        forward(&tx, failure_from_response(resp).await).await;
        return;
    }
    let mut translator = Translator::new(provider.kind);
    let mut parser = SseParser::new();
    let mut body = resp.body;
    loop {
        let chunk = body.next().await;
        let frames = match chunk {
            Some(Ok(bytes)) => parser.push(&bytes),
            Some(Err(_)) => {
                forward(
                    &tx,
                    LlmEvent::Failed {
                        kind: FailKind::ProviderError,
                        message: "Provider stream failed".to_owned(),
                        usage: None,
                    },
                )
                .await;
                return;
            }
            None => {
                let tail = parser
                    .finish()
                    .map(|f| translator.translate(&f))
                    .unwrap_or_default();
                for ev in tail {
                    let terminal = matches!(ev, LlmEvent::Completed(_) | LlmEvent::Failed { .. });
                    if tx.send(ev).await.is_err() || terminal {
                        return;
                    }
                }
                forward(
                    &tx,
                    LlmEvent::Failed {
                        kind: FailKind::ProviderError,
                        message: "Provider stream ended without a terminal event".to_owned(),
                        usage: None,
                    },
                )
                .await;
                return;
            }
        };
        for f in frames {
            for ev in translator.translate(&f) {
                let terminal = matches!(ev, LlmEvent::Completed(_) | LlmEvent::Failed { .. });
                if tx.send(ev).await.is_err() || terminal {
                    return;
                }
            }
        }
    }
}

/// Non-streaming call (thread summary). Returns the output text and usage.
///
/// # Errors
/// Returns `(kind, message)` on failure.
pub async fn complete_chat(
    transport: &dyn ProviderTransport,
    ctx: SecurityContext,
    provider: &ResolvedProvider,
    req: &ChatRequest,
) -> Result<(String, Option<UsageTokens>), (FailKind, String)> {
    let request = build_provider_request(provider, req);
    let resp =
        transport
            .send(ctx, request)
            .await
            .map_err(|e| match failure_from_transport(&e) {
                LlmEvent::Failed { kind, message, .. } => (kind, message),
                _ => (FailKind::ProviderError, "provider error".to_owned()),
            })?;
    if !resp.is_success() {
        return match failure_from_response(resp).await {
            LlmEvent::Failed { kind, message, .. } => Err((kind, message)),
            _ => Err((FailKind::ProviderError, "provider error".to_owned())),
        };
    }
    let ct = resp.content_type();
    let bytes = resp
        .into_bytes()
        .await
        .map_err(|e| (FailKind::ProviderError, e))?;
    if ct.contains("text/event-stream") {
        // Some providers stream even for non-stream requests: collect the text.
        let mut parser = SseParser::new();
        let mut t = Translator::new(provider.kind);
        let mut text = String::new();
        let mut frames = parser.push(&bytes);
        frames.extend(parser.finish());
        for f in frames {
            for ev in t.translate(&f) {
                match ev {
                    LlmEvent::Text(s) => text.push_str(&s),
                    LlmEvent::Completed(c) => {
                        let full = if text.is_empty() {
                            c.output_text.unwrap_or_default()
                        } else {
                            text
                        };
                        return Ok((full, c.usage));
                    }
                    LlmEvent::Failed { kind, message, .. } => return Err((kind, message)),
                    _ => {}
                }
            }
        }
        return Ok((text, None));
    }
    let v: Value =
        serde_json::from_slice(&bytes).map_err(|e| (FailKind::ProviderError, e.to_string()))?;
    let usage = v.get("usage").and_then(parse_usage);
    let text = match provider.kind {
        ProviderKind::OpenaiChatCompletions => v
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        ProviderKind::AnthropicMessages => v
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => v
            .get("output_text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| citations_from_response(&v).1)
            .unwrap_or_default(),
    };
    Ok((text, usage))
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod tests;
