//! Provider adapter: request building, SSE event translation and error mapping
//! (`OpenAI` Responses API wire format; other kinds filter tools per ADR-0005).
//!
//! DESIGN §3.3 "Provider Event Translation", §4 "Provider Request Metadata", ADR-0005.

use std::collections::HashMap;

use mini_chat_sdk::{ModelApiParams, UsageTokens, WebSearchContextSize};
use serde_json::{Map, Value, json};

use crate::config::ProviderKind;
use crate::infra::llm::transport::{HttpResponse, RawSseEvent, TransportError};

/// Role of a conversation input item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRole {
    User,
    Assistant,
}

/// One input message. Images are provider file ids (`input_image.file_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputItem {
    pub role: InputRole,
    pub text: String,
    pub image_file_ids: Vec<String>,
}

/// Built-in tool sent to the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSpec {
    FileSearch {
        vector_store_ids: Vec<String>,
        max_num_results: u32,
    },
    WebSearch {
        context_size: WebSearchContextSize,
    },
    CodeInterpreter {
        file_ids: Vec<String>,
    },
}

/// `metadata` object (`OpenAI` Responses adapter only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    /// `chat` | `summary`
    pub request_type: String,
    /// `none` | `file_search` | `web_search` | `code_interpreter` | combinations joined with `+`
    pub feature: String,
}

/// Provider-agnostic chat request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// `provider_model_id`.
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputItem>,
    pub max_output_tokens: i64,
    pub tools: Vec<ToolSpec>,
    /// `{tenant_hex}{user_hex}` (64 chars) or `{tenant}:{user}` fallback.
    pub user: String,
    pub metadata: RequestMetadata,
    pub api_params: ModelApiParams,
    /// Catalog `max_tool_calls` (sent only with tools, `OpenAI` Responses only).
    pub max_tool_calls: u32,
    pub stream: bool,
}

/// Internal provider events produced by the parser.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// Tool started (`file_search`, `web_search`, `code_interpreter`).
    ToolStart {
        name: String,
        details: serde_json::Value,
    },
    /// Tool finished.
    ToolDone {
        name: String,
        details: serde_json::Value,
    },
    /// Citation-bearing annotation of the output text.
    Annotation(Annotation),
    /// Terminal success (`response.completed`, or `response.incomplete` with a reason).
    Completed {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    /// Terminal provider error (`response.failed` / `error`); `message` is NOT yet sanitized.
    Failed {
        code: String,
        message: String,
        usage: Option<UsageTokens>,
    },
}

/// Output text annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    /// Web citation; `snippet` = annotation text or the output text in `[start, end)` (chars).
    Url {
        url: String,
        title: String,
        snippet: String,
        start: Option<usize>,
        end: Option<usize>,
    },
    /// File citation (provider file id; mapped to an attachment by the stream service).
    File {
        file_id: String,
        filename: Option<String>,
    },
}

/// A provider failure outside the SSE stream (HTTP status / transport).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    /// `provider_error` | `provider_timeout` | `rate_limited`
    pub code: String,
    /// Unsanitized provider message.
    pub message: String,
    pub retry_after_secs: Option<u64>,
}

/// Result of a non-streaming call (thread summary).
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionResult {
    pub text: String,
    pub usage: Option<UsageTokens>,
    pub response_id: Option<String>,
}

/// Keys of `extra_body` that the request itself controls (ignored with a warning).
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

/// Cap of the code interpreter output carried by the `tool` done event.
const CODE_OUTPUT_MAX_CHARS: usize = 8192;
const TRUNCATED_SUFFIX: &str = "...[truncated]";
/// Cap of a raw (non-JSON) provider error body used as a message.
const RAW_ERROR_MAX_CHARS: usize = 500;

/// Builds the JSON body for the provider kind (tools filtered per adapter: chat completions
/// drops `file_search` / `web_search` / `code_interpreter`, vLLM drops all tools, Anthropic drops `file_search`).
#[must_use]
pub fn build_request_body(kind: ProviderKind, req: &ChatRequest) -> serde_json::Value {
    match kind {
        ProviderKind::OpenaiResponses => responses_body(req, true),
        ProviderKind::VllmResponses => responses_body(req, false),
        ProviderKind::OpenaiChatCompletions => chat_completions_body(req),
        ProviderKind::AnthropicMessages => anthropic_body(req),
    }
}

fn responses_input(req: &ChatRequest) -> Vec<Value> {
    req.input
        .iter()
        .map(|item| match item.role {
            InputRole::User => {
                let mut content = vec![json!({"type": "input_text", "text": item.text})];
                content.extend(item.image_file_ids.iter().map(|id| json!({"type": "input_image", "file_id": id})));
                json!({"role": "user", "content": content})
            }
            InputRole::Assistant => {
                json!({"role": "assistant", "content": [{"type": "output_text", "text": item.text}]})
            }
        })
        .collect()
}

fn responses_tool(tool: &ToolSpec) -> Value {
    match tool {
        ToolSpec::FileSearch {
            vector_store_ids,
            max_num_results,
        } => json!({
            "type": "file_search",
            "vector_store_ids": vector_store_ids,
            "max_num_results": max_num_results,
        }),
        ToolSpec::WebSearch { context_size } => {
            json!({"type": "web_search", "search_context_size": web_search_size(*context_size)})
        }
        ToolSpec::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": {"type": "auto", "file_ids": file_ids},
        }),
    }
}

fn web_search_size(size: WebSearchContextSize) -> &'static str {
    size.as_str()
}

/// `OpenAI` Responses (`full = true`) and vLLM Responses (`full = false`: no tools, include,
/// `max_tool_calls` or metadata).
fn responses_body(req: &ChatRequest, full: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("input".into(), Value::Array(responses_input(req)));
    if !req.instructions.is_empty() {
        body.insert("instructions".into(), json!(req.instructions));
    }
    body.insert("stream".into(), json!(req.stream));
    body.insert("store".into(), json!(false));
    body.insert("max_output_tokens".into(), json!(req.max_output_tokens));
    if !req.user.is_empty() {
        body.insert("user".into(), json!(req.user));
    }
    if full {
        let m = &req.metadata;
        body.insert(
            "metadata".into(),
            json!({
                "tenant_id": m.tenant_id,
                "user_id": m.user_id,
                "chat_id": m.chat_id,
                "request_type": m.request_type,
                "feature": m.feature,
            }),
        );
        if !req.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(req.tools.iter().map(responses_tool).collect()),
            );
            body.insert("max_tool_calls".into(), json!(req.max_tool_calls));
            if req
                .tools
                .iter()
                .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
            {
                body.insert("include".into(), json!(["code_interpreter_call.outputs"]));
            }
        }
    }
    insert_sampling(&mut body, &req.api_params, true, "stop");
    if let Some(effort) = req
        .api_params
        .reasoning_effort
        .as_deref()
        .filter(|e| !e.is_empty())
    {
        body.insert("reasoning".into(), json!({"effort": effort}));
    }
    merge_extra_body(&mut body, &req.api_params);
    Value::Object(body)
}

fn chat_completions_body(req: &ChatRequest) -> Value {
    let mut messages = Vec::with_capacity(req.input.len() + 1);
    if !req.instructions.is_empty() {
        messages.push(json!({"role": "system", "content": req.instructions}));
    }
    for item in &req.input {
        let role = match item.role {
            InputRole::User => "user",
            InputRole::Assistant => "assistant",
        };
        // Provider file ids cannot be referenced by Chat Completions image parts: images are omitted.
        messages.push(json!({"role": role, "content": item.text}));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(req.stream));
    if req.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    body.insert("max_completion_tokens".into(), json!(req.max_output_tokens));
    if !req.user.is_empty() {
        body.insert("user".into(), json!(req.user));
    }
    insert_sampling(&mut body, &req.api_params, true, "stop");
    if let Some(effort) = req
        .api_params
        .reasoning_effort
        .as_deref()
        .filter(|e| !e.is_empty())
    {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    merge_extra_body(&mut body, &req.api_params);
    Value::Object(body)
}

fn anthropic_body(req: &ChatRequest) -> Value {
    let messages: Vec<Value> =
        req.input
            .iter()
            .map(|item| {
                let role = match item.role {
                    InputRole::User => "user",
                    InputRole::Assistant => "assistant",
                };
                let mut content = Vec::new();
                if item.role == InputRole::User {
                    content.extend(item.image_file_ids.iter().map(
                        |id| json!({"type": "image", "source": {"type": "file", "file_id": id}}),
                    ));
                }
                if !item.text.is_empty() || content.is_empty() {
                    content.push(json!({"type": "text", "text": item.text}));
                }
                json!({"role": role, "content": content})
            })
            .collect();
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("system".into(), json!(req.instructions));
    }
    body.insert("messages".into(), Value::Array(messages));
    body.insert("max_tokens".into(), json!(req.max_output_tokens));
    body.insert("stream".into(), json!(req.stream));
    if !req.user.is_empty() {
        body.insert("metadata".into(), json!({"user_id": req.user}));
    }
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter_map(|t| match t {
            ToolSpec::FileSearch { .. } => None,
            ToolSpec::WebSearch { .. } => {
                Some(json!({"type": "web_search_20250305", "name": "web_search"}))
            }
            ToolSpec::CodeInterpreter { .. } => {
                Some(json!({"type": "code_execution_20250522", "name": "code_execution"}))
            }
        })
        .collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    insert_sampling(&mut body, &req.api_params, false, "stop_sequences");
    Value::Object(body)
}

/// Sampling parameters, each only when set; `stop` only when non-empty.
fn insert_sampling(
    body: &mut Map<String, Value>,
    p: &ModelApiParams,
    penalties: bool,
    stop_key: &str,
) {
    if let Some(v) = p.temperature {
        body.insert("temperature".into(), json!(v));
    }
    if let Some(v) = p.top_p {
        body.insert("top_p".into(), json!(v));
    }
    if penalties {
        if let Some(v) = p.frequency_penalty {
            body.insert("frequency_penalty".into(), json!(v));
        }
        if let Some(v) = p.presence_penalty {
            body.insert("presence_penalty".into(), json!(v));
        }
    }
    if !p.stop.is_empty() {
        body.insert(stop_key.into(), json!(p.stop));
    }
}

/// Merges `extra_body` at the top level, skipping keys the request controls.
fn merge_extra_body(body: &mut Map<String, Value>, p: &ModelApiParams) {
    let Some(extra) = &p.extra_body else { return };
    for (k, v) in extra {
        if CONTROLLED_KEYS.contains(&k.as_str()) {
            tracing::warn!(key = %k, "ignoring extra_body key controlled by the request");
            continue;
        }
        body.insert(k.clone(), v.clone());
    }
}

/// `user` field value: `{tenant_simple}{user_simple}`.
#[must_use]
pub fn provider_user_field(tenant_id: uuid::Uuid, user_id: uuid::Uuid) -> String {
    format!("{}{}", tenant_id.simple(), user_id.simple())
}

// ───────────────────────────── stream parsing ─────────────────────────────

/// Incremental SSE translator (event name from `event:` or `data.type`).
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent stream state flags"
)]
pub struct StreamParser {
    kind: ProviderKind,
    /// A terminal event was emitted; later events are ignored.
    finished: bool,
    response_id: Option<String>,
    /// Output text per `(output_index, content_index)` (annotation range snippets).
    parts: HashMap<(u64, u64), String>,
    saw_annotation_added: bool,
    emitted_annotations: Vec<Annotation>,
    think: ThinkFilter,
    /// Chat Completions / Anthropic: usage and stop reason collected until the terminal event.
    usage: Option<UsageTokens>,
    incomplete_reason: Option<String>,
    finish_seen: bool,
    /// Anthropic: content block index → internal tool name.
    tool_blocks: HashMap<u64, String>,
}

impl Default for StreamParser {
    fn default() -> Self {
        Self::new(ProviderKind::OpenaiResponses)
    }
}

impl StreamParser {
    #[must_use]
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            finished: false,
            response_id: None,
            parts: HashMap::new(),
            saw_annotation_added: false,
            emitted_annotations: Vec::new(),
            think: ThinkFilter::default(),
            usage: None,
            incomplete_reason: None,
            finish_seen: false,
            tool_blocks: HashMap::new(),
        }
    }

    /// Translates one raw SSE event into zero or more internal events.
    pub fn on_event(&mut self, raw: &RawSseEvent) -> Vec<ProviderEvent> {
        if self.finished {
            return Vec::new();
        }
        match self.kind {
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => self.on_responses(raw),
            ProviderKind::OpenaiChatCompletions => self.on_chat_completions(raw),
            ProviderKind::AnthropicMessages => self.on_anthropic(raw),
        }
    }

    /// End of the provider stream without a terminal event: Chat Completions streams that ended
    /// after a `finish_reason` without `[DONE]` are completed; otherwise nothing is emitted.
    pub fn finish(&mut self) -> Vec<ProviderEvent> {
        if self.finished || self.kind != ProviderKind::OpenaiChatCompletions || !self.finish_seen {
            return Vec::new();
        }
        self.complete_collected()
    }

    /// Whether a terminal event (`Completed` / `Failed`) was emitted.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn complete_collected(&mut self) -> Vec<ProviderEvent> {
        self.finished = true;
        vec![ProviderEvent::Completed {
            usage: self.usage,
            response_id: self.response_id.clone(),
            incomplete_reason: self.incomplete_reason.clone(),
        }]
    }

    fn fail(&mut self, message: String, usage: Option<UsageTokens>) -> Vec<ProviderEvent> {
        self.finished = true;
        vec![ProviderEvent::Failed {
            code: "provider_error".to_owned(),
            message,
            usage,
        }]
    }

    fn push_annotation(&mut self, a: Annotation, out: &mut Vec<ProviderEvent>) {
        if self.emitted_annotations.contains(&a) {
            return;
        }
        self.emitted_annotations.push(a.clone());
        out.push(ProviderEvent::Annotation(a));
    }

    // ── OpenAI / vLLM Responses ──

    fn on_responses(&mut self, raw: &RawSseEvent) -> Vec<ProviderEvent> {
        let data: Option<Value> = serde_json::from_str(&raw.data).ok();
        let name = event_name(raw, data.as_ref());
        if name == "error" {
            let (message, usage) = error_event_message(&raw.data, data.as_ref());
            let mut out = self.flush_think();
            out.extend(self.fail(message, usage));
            return out;
        }
        let Some(d) = data else { return Vec::new() };
        let mut out = Vec::new();
        match name.as_str() {
            "response.created" | "response.in_progress" => {
                if let Some(id) = d.pointer("/response/id").and_then(Value::as_str) {
                    self.response_id = Some(id.to_owned());
                }
            }
            "response.output_text.delta" => {
                let delta = d.get("delta").and_then(Value::as_str).unwrap_or_default();
                if delta.is_empty() {
                    return out;
                }
                self.parts.entry(part_key(&d)).or_default().push_str(delta);
                if self.kind == ProviderKind::VllmResponses {
                    self.think.push(delta, &mut out);
                } else {
                    out.push(ProviderEvent::TextDelta(delta.to_owned()));
                }
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta"
                if self.kind == ProviderKind::VllmResponses =>
            {
                if let Some(delta) = d
                    .get("delta")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    out.push(ProviderEvent::ReasoningDelta(delta.to_owned()));
                }
            }
            "response.file_search_call.searching" => out.push(tool_start("file_search")),
            "response.file_search_call.completed" => {
                let n = d
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                out.push(ProviderEvent::ToolDone {
                    name: "file_search".to_owned(),
                    details: json!({"files_searched": n}),
                });
            }
            "response.web_search_call.searching" => out.push(tool_start("web_search")),
            "response.web_search_call.completed" => {
                out.push(ProviderEvent::ToolDone {
                    name: "web_search".to_owned(),
                    details: json!({}),
                });
            }
            "response.code_interpreter_call.in_progress" => {
                out.push(tool_start("code_interpreter"))
            }
            "response.output_item.done" => {
                let item = d.get("item").unwrap_or(&Value::Null);
                if item.get("type").and_then(Value::as_str) == Some("code_interpreter_call") {
                    out.push(ProviderEvent::ToolDone {
                        name: "code_interpreter".to_owned(),
                        details: json!({"output": code_interpreter_output(item)}),
                    });
                }
            }
            "response.output_text.annotation.added" => {
                self.saw_annotation_added = true;
                let key = part_key(&d);
                let text = self.parts.get(&key).map(String::as_str);
                if let Some(a) = d
                    .get("annotation")
                    .and_then(|a| convert_annotation(a, text))
                {
                    self.push_annotation(a, &mut out);
                }
            }
            "response.completed" => {
                let resp = d.get("response").unwrap_or(&Value::Null);
                out.extend(self.flush_think());
                if !self.saw_annotation_added {
                    for a in self.final_annotations(resp) {
                        self.push_annotation(a, &mut out);
                    }
                }
                self.finished = true;
                out.push(ProviderEvent::Completed {
                    usage: parse_usage(resp.get("usage")),
                    response_id: self.final_response_id(resp),
                    incomplete_reason: None,
                });
            }
            "response.incomplete" => {
                let resp = d.get("response").unwrap_or(&Value::Null);
                out.extend(self.flush_think());
                let reason = resp
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .filter(|r| !r.is_empty())
                    .unwrap_or("other")
                    .to_owned();
                self.finished = true;
                out.push(ProviderEvent::Completed {
                    usage: parse_usage(resp.get("usage")),
                    response_id: self.final_response_id(resp),
                    incomplete_reason: Some(reason),
                });
            }
            "response.failed" => {
                out.extend(self.flush_think());
                let message = failed_message(&d).unwrap_or_else(|| "provider error".to_owned());
                out.extend(self.fail(message, parse_usage(d.pointer("/response/usage"))));
            }
            _ => {}
        }
        out
    }

    fn final_response_id(&self, resp: &Value) -> Option<String> {
        resp.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.response_id.clone())
    }

    fn flush_think(&mut self) -> Vec<ProviderEvent> {
        let mut out = Vec::new();
        if self.kind == ProviderKind::VllmResponses {
            self.think.flush(&mut out);
        }
        out
    }

    /// Annotations of the final `response.output[].content[]` (used when no
    /// `annotation.added` events were streamed).
    fn final_annotations(&self, resp: &Value) -> Vec<Annotation> {
        let mut found = Vec::new();
        let Some(output) = resp.get("output").and_then(Value::as_array) else {
            return found;
        };
        for (oi, item) in output.iter().enumerate() {
            let Some(content) = item.get("content").and_then(Value::as_array) else {
                continue;
            };
            for (ci, part) in content.iter().enumerate() {
                let Some(anns) = part.get("annotations").and_then(Value::as_array) else {
                    continue;
                };
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| self.parts.get(&(oi as u64, ci as u64)).map(String::as_str));
                found.extend(anns.iter().filter_map(|a| convert_annotation(a, text)));
            }
        }
        found
    }

    // ── Chat Completions ──

    fn on_chat_completions(&mut self, raw: &RawSseEvent) -> Vec<ProviderEvent> {
        let data = raw.data.trim();
        if data == "[DONE]" {
            return self.complete_collected();
        }
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        if raw.event.as_deref() == Some("error")
            || parsed.as_ref().is_some_and(|d| d.get("error").is_some())
        {
            let (message, usage) = error_event_message(data, parsed.as_ref());
            return self.fail(message, usage);
        }
        let Some(d) = parsed else { return Vec::new() };
        let mut out = Vec::new();
        if let Some(id) = d.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(u) = parse_usage(d.get("usage")) {
            self.usage = Some(u);
        }
        if let Some(choice) = d
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        {
            let delta = choice.get("delta").unwrap_or(&Value::Null);
            for key in ["reasoning_content", "reasoning"] {
                if let Some(r) = delta
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    out.push(ProviderEvent::ReasoningDelta(r.to_owned()));
                    break;
                }
            }
            if let Some(t) = delta
                .get("content")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                out.push(ProviderEvent::TextDelta(t.to_owned()));
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_seen = true;
                self.incomplete_reason = match reason {
                    "length" => Some("max_tokens".to_owned()),
                    "content_filter" => Some("content_filter".to_owned()),
                    _ => None,
                };
            }
        }
        out
    }

    // ── Anthropic Messages ──

    fn on_anthropic(&mut self, raw: &RawSseEvent) -> Vec<ProviderEvent> {
        let data: Option<Value> = serde_json::from_str(&raw.data).ok();
        let name = event_name(raw, data.as_ref());
        if name == "error" {
            let (message, usage) = error_event_message(&raw.data, data.as_ref());
            return self.fail(message, usage);
        }
        let Some(d) = data else { return Vec::new() };
        let mut out = Vec::new();
        match name.as_str() {
            "message_start" => {
                let msg = d.get("message").unwrap_or(&Value::Null);
                if let Some(id) = msg.get("id").and_then(Value::as_str) {
                    self.response_id = Some(id.to_owned());
                }
                if let Some(u) = msg.get("usage").filter(|u| u.is_object()) {
                    self.merge_anthropic_usage(u);
                }
            }
            "content_block_start" => {
                let block = d.get("content_block").unwrap_or(&Value::Null);
                if block.get("type").and_then(Value::as_str) == Some("server_tool_use") {
                    let tool = match block.get("name").and_then(Value::as_str) {
                        Some("web_search") => Some("web_search"),
                        Some(
                            "code_execution" | "bash_code_execution" | "text_editor_code_execution",
                        ) => Some("code_interpreter"),
                        _ => None,
                    };
                    if let Some(tool) = tool {
                        let index = d.get("index").and_then(Value::as_u64).unwrap_or(0);
                        self.tool_blocks.insert(index, tool.to_owned());
                        out.push(tool_start(tool));
                    }
                }
            }
            "content_block_delta" => {
                let delta = d.get("delta").unwrap_or(&Value::Null);
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            out.push(ProviderEvent::TextDelta(t.to_owned()));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = delta
                            .get("thinking")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                        {
                            out.push(ProviderEvent::ReasoningDelta(t.to_owned()));
                        }
                    }
                    Some("citations_delta") => {
                        let c = delta.get("citation").unwrap_or(&Value::Null);
                        if let Some(url) = c.get("url").and_then(Value::as_str) {
                            let a = Annotation::Url {
                                url: url.to_owned(),
                                title: c
                                    .get("title")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                snippet: c
                                    .get("cited_text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                start: None,
                                end: None,
                            };
                            self.push_annotation(a, &mut out);
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = d.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(tool) = self.tool_blocks.remove(&index) {
                    out.push(ProviderEvent::ToolDone {
                        name: tool,
                        details: json!({}),
                    });
                }
            }
            "message_delta" => {
                if let Some(u) = d.get("usage").filter(|u| u.is_object()) {
                    self.merge_anthropic_usage(u);
                }
                if let Some(reason) = d.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.finish_seen = true;
                    self.incomplete_reason = match reason {
                        "max_tokens" => Some("max_tokens".to_owned()),
                        "refusal" => Some("content_filter".to_owned()),
                        _ => None,
                    };
                }
            }
            "message_stop" => out.extend(self.complete_collected()),
            _ => {}
        }
        out
    }

    /// Anthropic reports `input_tokens` without cache reads/writes; the internal usage counts all
    /// input tokens (as `OpenAI` does) and keeps the cache counters separately.
    fn merge_anthropic_usage(&mut self, u: &Value) {
        let mut cur = self.usage.unwrap_or_default();
        let get = |k: &str| u.get(k).and_then(Value::as_i64);
        let cache_read = get("cache_read_input_tokens");
        let cache_write = get("cache_creation_input_tokens");
        if let Some(r) = cache_read {
            cur.cache_read_input_tokens = r;
        }
        if let Some(w) = cache_write {
            cur.cache_write_input_tokens = w;
        }
        if let Some(i) = get("input_tokens") {
            cur.input_tokens = i + cur.cache_read_input_tokens + cur.cache_write_input_tokens;
        }
        if let Some(o) = get("output_tokens") {
            cur.output_tokens = o;
        }
        self.usage = Some(cur);
    }
}

fn tool_start(name: &str) -> ProviderEvent {
    ProviderEvent::ToolStart {
        name: name.to_owned(),
        details: json!({}),
    }
}

/// Event name: the SSE `event:` line unless missing or `message`, else the data `type`.
fn event_name(raw: &RawSseEvent, data: Option<&Value>) -> String {
    match raw.event.as_deref() {
        Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
        _ => data
            .and_then(|d| d.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

fn part_key(d: &Value) -> (u64, u64) {
    (
        d.get("output_index").and_then(Value::as_u64).unwrap_or(0),
        d.get("content_index").and_then(Value::as_u64).unwrap_or(0),
    )
}

/// `logs` outputs of a code interpreter item joined with `\n`, capped at 8192 characters.
fn code_interpreter_output(item: &Value) -> String {
    let logs: Vec<&str> = item
        .get("outputs")
        .and_then(Value::as_array)
        .map(|outs| {
            outs.iter()
                .filter(|o| o.get("type").and_then(Value::as_str) == Some("logs"))
                .filter_map(|o| o.get("logs").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let joined = logs.join("\n");
    if joined.chars().count() > CODE_OUTPUT_MAX_CHARS {
        let mut s: String = joined.chars().take(CODE_OUTPUT_MAX_CHARS).collect();
        s.push_str(TRUNCATED_SUFFIX);
        s
    } else {
        joined
    }
}

fn index_of(a: &Value, key: &str) -> Option<usize> {
    a.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
}

/// Converts a Responses annotation; `text` is the output text part carrying it.
fn convert_annotation(a: &Value, text: Option<&str>) -> Option<Annotation> {
    match a.get("type").and_then(Value::as_str)? {
        "url_citation" => {
            let url = a.get("url").and_then(Value::as_str)?.to_owned();
            let title = a
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let start = index_of(a, "start_index");
            let end = index_of(a, "end_index");
            let snippet = match a.get("text").and_then(Value::as_str) {
                Some(t) => t.to_owned(),
                None => match (start, end, text) {
                    (Some(s), Some(e), Some(t)) if s <= e && e <= t.chars().count() => {
                        t.chars().skip(s).take(e - s).collect()
                    }
                    _ => String::new(),
                },
            };
            Some(Annotation::Url {
                url,
                title,
                snippet,
                start,
                end,
            })
        }
        "file_citation" => {
            let file_id = a.get("file_id").and_then(Value::as_str)?.to_owned();
            let filename = a.get("filename").and_then(Value::as_str).map(str::to_owned);
            Some(Annotation::File { file_id, filename })
        }
        _ => None,
    }
}

/// Message of an error object: a plain string, or its `message` (else `code`).
fn error_object_message(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Object(o) => o
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                o.get("code")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .map(str::to_owned),
        _ => None,
    }
}

/// `response.error` with top-level `error` as fallback.
fn failed_message(d: &Value) -> Option<String> {
    d.pointer("/response/error")
        .and_then(error_object_message)
        .or_else(|| d.get("error").and_then(error_object_message))
}

/// SSE `error` event: parsed like `response.failed`, then as flat `{code, message}`;
/// unparseable data becomes the message.
fn error_event_message(raw: &str, data: Option<&Value>) -> (String, Option<UsageTokens>) {
    let Some(d) = data else {
        let raw = raw.trim();
        let msg = if raw.is_empty() {
            "provider error".to_owned()
        } else {
            truncate_chars(raw, RAW_ERROR_MAX_CHARS)
        };
        return (msg, None);
    };
    let message = failed_message(d)
        .or_else(|| error_object_message(d))
        .unwrap_or_else(|| "provider error".to_owned());
    (message, parse_usage(d.pointer("/response/usage")))
}

/// Provider usage → internal usage (Responses, Chat Completions and Anthropic field names).
/// Missing fields count as 0; a missing usage object is `None`.
fn parse_usage(v: Option<&Value>) -> Option<UsageTokens> {
    let o = v?.as_object()?;
    let int = |v: Option<&Value>| v.and_then(Value::as_i64).unwrap_or(0);
    let input_details = o
        .get("input_tokens_details")
        .or_else(|| o.get("prompt_tokens_details"));
    let output_details = o
        .get("output_tokens_details")
        .or_else(|| o.get("completion_tokens_details"));
    let cached = input_details
        .and_then(|d| d.get("cached_tokens"))
        .or_else(|| o.get("cache_read_input_tokens"));
    Some(UsageTokens {
        input_tokens: int(o.get("input_tokens").or_else(|| o.get("prompt_tokens"))),
        output_tokens: int(o
            .get("output_tokens")
            .or_else(|| o.get("completion_tokens"))),
        cache_read_input_tokens: int(cached),
        cache_write_input_tokens: int(o.get("cache_creation_input_tokens")),
        reasoning_tokens: int(output_details.and_then(|d| d.get("reasoning_tokens"))),
    })
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect()
    } else {
        s.to_owned()
    }
}

/// Splits vLLM `<think>…</think>` text into reasoning and answer deltas (tags may span chunks).
#[derive(Debug, Default)]
struct ThinkFilter {
    in_think: bool,
    pending: String,
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

impl ThinkFilter {
    fn push(&mut self, delta: &str, out: &mut Vec<ProviderEvent>) {
        self.pending.push_str(delta);
        loop {
            let tag = if self.in_think {
                THINK_CLOSE
            } else {
                THINK_OPEN
            };
            if let Some(pos) = self.pending.find(tag) {
                let before: String = self.pending.drain(..pos).collect();
                self.emit(before, out);
                self.pending.drain(..tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            let keep = partial_tag_suffix(&self.pending, tag);
            let cut = self.pending.len() - keep;
            let ready: String = self.pending.drain(..cut).collect();
            self.emit(ready, out);
            break;
        }
    }

    fn flush(&mut self, out: &mut Vec<ProviderEvent>) {
        let rest = std::mem::take(&mut self.pending);
        self.emit(rest, out);
    }

    fn emit(&self, s: String, out: &mut Vec<ProviderEvent>) {
        if s.is_empty() {
            return;
        }
        out.push(if self.in_think {
            ProviderEvent::ReasoningDelta(s)
        } else {
            ProviderEvent::TextDelta(s)
        });
    }
}

/// Length of the longest suffix of `s` that is a proper prefix of `tag`.
fn partial_tag_suffix(s: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&k| {
            s.len() >= k && s.is_char_boundary(s.len() - k) && tag.starts_with(&s[s.len() - k..])
        })
        .unwrap_or(0)
}

// ───────────────────────────── HTTP errors / non-streaming ─────────────────────────────

/// Error message of a provider error body: `error.message` / `error` string / `message` /
/// Problem `detail`.
fn body_error_message(json: &Value) -> Option<String> {
    json.get("error")
        .and_then(error_object_message)
        .or_else(|| {
            json.get("message")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .or_else(|| {
            json.get("detail")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
}

/// Maps a non-2xx provider response (429 → `rate_limited`; OAGW 504 deadline → `provider_timeout`;
/// anything else → `provider_error`, message from the error body).
#[must_use]
pub fn map_http_error(resp: &HttpResponse) -> ProviderFailure {
    if resp.status == 429 {
        let message = match resp.retry_after_secs {
            Some(n) => format!("Rate limited by provider (retry after {n} s)"),
            None => "Rate limited by provider".to_owned(),
        };
        return ProviderFailure {
            code: "rate_limited".to_owned(),
            message,
            retry_after_secs: resp.retry_after_secs,
        };
    }
    let json = resp.json();
    let is_deadline_problem = resp.status == 504
        && json.get("error").is_none()
        && resp.body.windows(17).any(|w| w == b"deadline_exceeded");
    if is_deadline_problem {
        return ProviderFailure {
            code: "provider_timeout".to_owned(),
            message: "Provider request timed out".to_owned(),
            retry_after_secs: resp.retry_after_secs,
        };
    }
    let message = body_error_message(&json)
        .or_else(|| {
            let raw = String::from_utf8_lossy(&resp.body);
            let raw = raw.trim();
            (!raw.is_empty() && !json.is_object()).then(|| truncate_chars(raw, RAW_ERROR_MAX_CHARS))
        })
        .unwrap_or_else(|| format!("Provider returned HTTP {}", resp.status));
    ProviderFailure {
        code: "provider_error".to_owned(),
        message,
        retry_after_secs: resp.retry_after_secs,
    }
}

/// Maps a transport error (`Timeout` → `provider_timeout`, else `provider_error`).
#[must_use]
pub fn map_transport_error(err: &TransportError) -> ProviderFailure {
    match err {
        TransportError::Timeout(m) => ProviderFailure {
            code: "provider_timeout".to_owned(),
            message: format!("Provider request timed out: {m}"),
            retry_after_secs: None,
        },
        TransportError::Other(m) => ProviderFailure {
            code: "provider_error".to_owned(),
            message: m.clone(),
            retry_after_secs: None,
        },
    }
}

fn invalid_response(message: &str) -> ProviderFailure {
    ProviderFailure {
        code: "provider_error".to_owned(),
        message: message.to_owned(),
        retry_after_secs: None,
    }
}

/// Parses a non-streaming response body into text + usage.
///
/// # Errors
/// `ProviderFailure` when the body is not a successful completion.
pub fn parse_completion(
    kind: ProviderKind,
    resp: &HttpResponse,
) -> Result<CompletionResult, ProviderFailure> {
    if !resp.is_success() {
        return Err(map_http_error(resp));
    }
    let json = resp.json();
    if !json.is_object() {
        return Err(invalid_response("invalid provider response"));
    }
    let response_id = json.get("id").and_then(Value::as_str).map(str::to_owned);
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            if json.get("status").and_then(Value::as_str) == Some("failed") {
                let msg = json
                    .get("error")
                    .and_then(error_object_message)
                    .unwrap_or_else(|| "provider error".to_owned());
                return Err(invalid_response(&msg));
            }
            let mut text = String::new();
            for item in json
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        text.push_str(part.get("text").and_then(Value::as_str).unwrap_or_default());
                    }
                }
            }
            if text.is_empty()
                && let Some(t) = json.get("output_text").and_then(Value::as_str)
            {
                t.clone_into(&mut text);
            }
            if kind == ProviderKind::VllmResponses {
                text = strip_think(&text);
            }
            Ok(CompletionResult {
                text,
                usage: parse_usage(json.get("usage")),
                response_id,
            })
        }
        ProviderKind::OpenaiChatCompletions => {
            if let Some(msg) = json.get("error").and_then(error_object_message) {
                return Err(invalid_response(&msg));
            }
            let text = json
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Ok(CompletionResult {
                text,
                usage: parse_usage(json.get("usage")),
                response_id,
            })
        }
        ProviderKind::AnthropicMessages => {
            if let Some(msg) = json.get("error").and_then(error_object_message) {
                return Err(invalid_response(&msg));
            }
            let text: String = json
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            let mut parser = StreamParser::new(ProviderKind::AnthropicMessages);
            if let Some(u) = json.get("usage").filter(|u| u.is_object()) {
                parser.merge_anthropic_usage(u);
            }
            Ok(CompletionResult {
                text,
                usage: parser.usage,
                response_id,
            })
        }
    }
}

/// Removes `<think>…</think>` blocks from a complete text.
fn strip_think(text: &str) -> String {
    let mut f = ThinkFilter::default();
    let mut events = Vec::new();
    f.push(text, &mut events);
    f.flush(&mut events);
    events
        .into_iter()
        .filter_map(|e| match e {
            ProviderEvent::TextDelta(t) => Some(t),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "responses_tests.rs"]
mod tests;
