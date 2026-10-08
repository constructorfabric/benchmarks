//! Anthropic Messages adapter (`/v1/messages`; DESIGN section 3.2
//! `llm_provider`, section 3.3 "Provider Event Translation" and tool events,
//! section 4 "Provider Request Metadata").
//!
//! Request:
//! - the system prompt is `system`; `system`-role input messages are appended
//!   to it; the caller identity is `metadata.user_id` (the provider `user`
//!   value); `extra_body`, `max_tool_calls`, `frequency_penalty`,
//!   `presence_penalty` and `reasoning_effort` are not sent; `stop` is
//!   `stop_sequences`;
//! - `file_search` is dropped; `web_search` is the `web_search_20250305` server
//!   tool and `code_interpreter` the `code_execution_20250825` server tool
//!   (with its `anthropic-beta` header; uploaded files are not mounted);
//! - image parts are dropped: Anthropic needs its own file ids (the secondary
//!   image copy), which the request does not carry.
//!
//! Stream (`message_start`, `content_block_start` / `_delta` / `_stop`,
//! `message_delta`, `message_stop`, `error`):
//! - `server_tool_use` `web_search` starts a `web_search` tool event and its
//!   `web_search_tool_result` block ends it; a code execution
//!   `server_tool_use` block maps to `code_interpreter` start / done
//!   (`details: {}`);
//! - a client `tool_use` block emits a `start` event named `search_knowledge`,
//!   `load_files` or `unknown_tool` (no `done`) and, when the block stops,
//!   [`LlmEvent::FunctionCall`] with the accumulated JSON input;
//! - `citations_delta` web search locations are web citations, sent before
//!   `Completed`;
//! - `stop_reason: "max_tokens"` (and `pause_turn`) is an incomplete
//!   completion;
//! - usage: `input_tokens` is the uncached input plus cache reads and cache
//!   writes (`cache_read_input_tokens`, `cache_creation_input_tokens`).

use std::collections::HashMap;

use mini_chat_sdk::UsageTokens;
use serde_json::{Map, Value, json};

use super::WireRequest;
use super::error_from_value;
use super::stream::Translate;
use super::wire::{array, str_field};
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::llm::sse::SseFrame;
use crate::infra::llm::types::{
    CompletionResult, ContentPart, InputItem, LlmEvent, LlmRequest, ProviderError, RawCitation,
    ToolSpec,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const CODE_EXECUTION_BETA: &str = "code-execution-2025-08-25";
const WEB_SEARCH_TOOL: &str = "web_search_20250305";
const CODE_EXECUTION_TOOL: &str = "code_execution_20250825";

const WEB_SEARCH: &str = "web_search";
const CODE_INTERPRETER: &str = "code_interpreter";
/// `server_tool_use` names of the code execution tool versions.
const CODE_EXECUTION_NAMES: &[&str] = &[
    "code_execution",
    "bash_code_execution",
    "text_editor_code_execution",
];
/// Function tool names reported by name in `tool` events.
const KNOWN_FUNCTIONS: &[&str] = &["search_knowledge", "load_files"];
const UNKNOWN_TOOL: &str = "unknown_tool";

// ── Request ──────────────────────────────────────────────────────────────────

/// The Messages API request and its headers.
pub(super) fn build_request(req: &LlmRequest) -> WireRequest {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(req.model));
    let (system, messages) = messages(req);
    if !system.is_empty() {
        body.insert("system".to_owned(), json!(system));
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    let tools: Vec<Value> = req.tools.iter().filter_map(tool).collect();
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    body.insert("max_tokens".to_owned(), json!(req.max_output_tokens));
    let p = &req.api_params;
    for (key, value) in [("temperature", p.temperature), ("top_p", p.top_p)] {
        if let Some(v) = value {
            body.insert(key.to_owned(), json!(v));
        }
    }
    if !p.stop.is_empty() {
        body.insert("stop_sequences".to_owned(), json!(p.stop));
    }
    body.insert("metadata".to_owned(), json!({"user_id": req.user}));
    body.insert("stream".to_owned(), json!(req.stream));

    let mut headers = vec![("anthropic-version", ANTHROPIC_VERSION.to_owned())];
    if req
        .tools
        .iter()
        .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
    {
        headers.push(("anthropic-beta", CODE_EXECUTION_BETA.to_owned()));
    }
    WireRequest {
        body: Value::Object(body),
        headers,
    }
}

/// `(system, messages)`: the instructions plus the text of `system`-role
/// items, and the conversation. Consecutive function calls share one
/// assistant message, consecutive outputs one user message.
fn messages(req: &LlmRequest) -> (String, Vec<Value>) {
    let mut system = vec![req.instructions.clone()];
    let mut out: Vec<Value> = Vec::with_capacity(req.input.len());
    for item in &req.input {
        match item {
            InputItem::Message { role, content } => {
                let blocks = text_blocks(content);
                if *role == "system" {
                    system.extend(blocks.iter().map(|b| str_field(b, "text").to_owned()));
                } else if !blocks.is_empty() {
                    out.push(json!({"role": role, "content": blocks}));
                }
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let input = serde_json::from_str::<Value>(arguments)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                let block =
                    json!({"type": "tool_use", "id": call_id, "name": name, "input": input});
                push_block(&mut out, "assistant", "tool_use", block);
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                let block =
                    json!({"type": "tool_result", "tool_use_id": call_id, "content": output});
                push_block(&mut out, "user", "tool_result", block);
            }
        }
    }
    let system = system
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (system, out)
}

/// Text parts as text blocks; image parts are dropped.
fn text_blocks(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::InputText(t) | ContentPart::OutputText(t) if !t.is_empty() => {
                Some(json!({"type": "text", "text": t}))
            }
            _ => None,
        })
        .collect()
}

/// Append `block` to the last message when it has `role` and ends with a
/// block of `kind`; otherwise start a new message.
fn push_block(out: &mut Vec<Value>, role: &str, kind: &str, block: Value) {
    let open = out
        .last_mut()
        .filter(|m| str_field(m, "role") == role)
        .and_then(|m| m.get_mut("content")?.as_array_mut())
        .filter(|blocks| blocks.last().is_some_and(|b| str_field(b, "type") == kind));
    match open {
        Some(blocks) => blocks.push(block),
        None => out.push(json!({"role": role, "content": [block]})),
    }
}

fn tool(spec: &ToolSpec) -> Option<Value> {
    match spec {
        ToolSpec::FileSearch { .. } => None,
        ToolSpec::WebSearch { .. } => Some(json!({"type": WEB_SEARCH_TOOL, "name": WEB_SEARCH})),
        ToolSpec::CodeInterpreter { .. } => {
            Some(json!({"type": CODE_EXECUTION_TOOL, "name": "code_execution"}))
        }
        ToolSpec::Function {
            name,
            description,
            parameters,
        } => Some(json!({"name": name, "description": description, "input_schema": parameters})),
    }
}

// ── Responses ────────────────────────────────────────────────────────────────

/// Raw Anthropic usage counters.
#[derive(Default, Clone, Copy)]
struct RawUsage {
    input: i64,
    cache_read: i64,
    cache_creation: i64,
    output: i64,
}

impl RawUsage {
    /// Overwrite the counters present in `v`.
    fn update(&mut self, v: Option<&Value>) {
        let Some(u) = v.and_then(Value::as_object) else {
            return;
        };
        for (key, slot) in [
            ("input_tokens", &mut self.input),
            ("cache_read_input_tokens", &mut self.cache_read),
            ("cache_creation_input_tokens", &mut self.cache_creation),
            ("output_tokens", &mut self.output),
        ] {
            if let Some(n) = u.get(key).and_then(Value::as_i64) {
                *slot = n;
            }
        }
    }

    fn normalized(self) -> UsageTokens {
        UsageTokens {
            input_tokens: self
                .input
                .saturating_add(self.cache_read)
                .saturating_add(self.cache_creation),
            output_tokens: self.output,
            cache_read_input_tokens: self.cache_read,
            cache_write_input_tokens: self.cache_creation,
            reasoning_tokens: 0,
        }
    }
}

fn parse_usage(v: Option<&Value>) -> Option<UsageTokens> {
    v.filter(|u| u.is_object())?;
    let mut raw = RawUsage::default();
    raw.update(v);
    Some(raw.normalized())
}

/// `{type: "error", error: {type, message}}` or a bare `{type, message}`.
fn error_of(v: &Value) -> Option<ProviderError> {
    error_from_value(v.get("error")).or_else(|| {
        (str_field(v, "type") == "error").then(|| ProviderError::provider("provider error"))
    })
}

/// Text and usage of a non-streaming reply.
pub(super) fn parse_completion(bytes: &[u8]) -> Result<CompletionResult, ProviderError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|_| ProviderError::provider("invalid provider response"))?;
    if let Some(e) = error_of(&v) {
        return Err(e);
    }
    let text: String = array(v.get("content"))
        .iter()
        .filter(|b| str_field(b, "type") == "text")
        .map(|b| str_field(b, "text"))
        .collect();
    Ok(CompletionResult {
        text,
        usage: parse_usage(v.get("usage")),
    })
}

fn incomplete_reason(stop_reason: Option<&str>) -> Option<String> {
    match stop_reason? {
        "max_tokens" => Some("max_tokens".to_owned()),
        "pause_turn" => Some("pause_turn".to_owned()),
        _ => None,
    }
}

// ── Streaming ────────────────────────────────────────────────────────────────

/// An open content block.
enum Block {
    Function {
        id: String,
        name: String,
        input: String,
    },
    CodeExecution,
    Other,
}

/// Per-stream translation state.
#[derive(Default)]
pub(super) struct Translator {
    message_id: Option<String>,
    usage: RawUsage,
    usage_seen: bool,
    blocks: HashMap<u64, Block>,
    citations: Vec<RawCitation>,
    stop_reason: Option<String>,
}

impl Translate for Translator {
    fn on_frame(&mut self, frame: &SseFrame) -> Vec<LlmEvent> {
        let data = frame.data.trim();
        if data.is_empty() {
            return Vec::new();
        }
        let parsed: Option<Value> = serde_json::from_str(data).ok();
        let name = match frame.event.as_deref() {
            Some(e) if !e.is_empty() && e != "message" => e.to_owned(),
            _ => parsed
                .as_ref()
                .map(|v| str_field(v, "type").to_owned())
                .unwrap_or_default(),
        };
        if name == "error" {
            let error = parsed
                .as_ref()
                .and_then(error_of)
                .unwrap_or_else(|| ProviderError::provider(sanitize_provider_message(data)));
            return vec![LlmEvent::Failed { error, usage: None }];
        }
        let Some(v) = parsed else {
            tracing::debug!(event = %name, "unparseable provider event ignored");
            return Vec::new();
        };
        self.on_event(&name, &v)
    }
}

impl Translator {
    fn on_event(&mut self, name: &str, v: &Value) -> Vec<LlmEvent> {
        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match name {
            "message_start" => {
                let message = v.get("message");
                self.message_id = message
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.on_usage(message.and_then(|m| m.get("usage")));
                Vec::new()
            }
            "content_block_start" => self.block_start(index, v.get("content_block")),
            "content_block_delta" => self.block_delta(index, v.get("delta")),
            "content_block_stop" => match self.blocks.remove(&index) {
                Some(Block::Function { id, name, input }) => vec![LlmEvent::FunctionCall {
                    call_id: id,
                    name,
                    arguments: input,
                }],
                Some(Block::CodeExecution) => vec![tool_event(CODE_INTERPRETER, false)],
                Some(Block::Other) | None => Vec::new(),
            },
            "message_delta" => {
                if let Some(reason) = v
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    self.stop_reason = Some(reason.to_owned());
                }
                self.on_usage(v.get("usage"));
                Vec::new()
            }
            "message_stop" => self.completed(),
            _ => Vec::new(),
        }
    }

    fn on_usage(&mut self, v: Option<&Value>) {
        if v.is_some_and(Value::is_object) {
            self.usage_seen = true;
            self.usage.update(v);
        }
    }

    fn block_start(&mut self, index: u64, block: Option<&Value>) -> Vec<LlmEvent> {
        let Some(block) = block else {
            return Vec::new();
        };
        let tool_name = str_field(block, "name");
        let (state, events) = match str_field(block, "type") {
            "tool_use" => {
                let event_name = if KNOWN_FUNCTIONS.contains(&tool_name) {
                    tool_name
                } else {
                    UNKNOWN_TOOL
                };
                let state = Block::Function {
                    id: str_field(block, "id").to_owned(),
                    name: tool_name.to_owned(),
                    input: String::new(),
                };
                (state, vec![tool_event(event_name, true)])
            }
            "server_tool_use" if tool_name == WEB_SEARCH => {
                (Block::Other, vec![tool_event(WEB_SEARCH, true)])
            }
            "server_tool_use" if CODE_EXECUTION_NAMES.contains(&tool_name) => (
                Block::CodeExecution,
                vec![tool_event(CODE_INTERPRETER, true)],
            ),
            "web_search_tool_result" => (Block::Other, vec![tool_event(WEB_SEARCH, false)]),
            _ => (Block::Other, Vec::new()),
        };
        self.blocks.insert(index, state);
        events
    }

    fn block_delta(&mut self, index: u64, delta: Option<&Value>) -> Vec<LlmEvent> {
        let Some(delta) = delta else {
            return Vec::new();
        };
        match str_field(delta, "type") {
            "text_delta" => {
                let text = str_field(delta, "text");
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![LlmEvent::TextDelta(text.to_owned())]
                }
            }
            "input_json_delta" => {
                if let Some(Block::Function { input, .. }) = self.blocks.get_mut(&index) {
                    input.push_str(str_field(delta, "partial_json"));
                }
                Vec::new()
            }
            "citations_delta" => {
                if let Some(c) = delta.get("citation").and_then(web_citation) {
                    self.citations.push(c);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn completed(&mut self) -> Vec<LlmEvent> {
        let incomplete_reason = incomplete_reason(self.stop_reason.as_deref());
        let mut events = Vec::new();
        let citations = std::mem::take(&mut self.citations);
        if incomplete_reason.is_none() && !citations.is_empty() {
            events.push(LlmEvent::Citations(citations));
        }
        events.push(LlmEvent::Completed {
            usage: self.usage_seen.then(|| self.usage.normalized()),
            response_id: self.message_id.clone(),
            incomplete_reason,
        });
        events
    }
}

fn tool_event(name: &str, start: bool) -> LlmEvent {
    let (name, details) = (name.to_owned(), json!({}));
    if start {
        LlmEvent::ToolStart { name, details }
    } else {
        LlmEvent::ToolDone { name, details }
    }
}

/// A `web_search_result_location` citation.
fn web_citation(c: &Value) -> Option<RawCitation> {
    if str_field(c, "type") != "web_search_result_location" {
        return None;
    }
    let url = c.get("url").and_then(Value::as_str)?;
    Some(RawCitation::Web {
        url: url.to_owned(),
        title: str_field(c, "title").to_owned(),
        snippet: str_field(c, "cited_text").to_owned(),
        span: None,
    })
}

#[cfg(test)]
#[path = "anthropic_messages_tests.rs"]
mod anthropic_messages_tests;
