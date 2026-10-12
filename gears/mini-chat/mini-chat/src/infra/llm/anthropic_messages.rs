//! `anthropic_messages` adapter: Anthropic Messages API (DESIGN section 3.2
//! `llm_provider`, section 3.3 `event: tool`, "Provider Event Translation",
//! "Provider Request Metadata", "Model Catalog Configuration", section 5.4
//! "Canonical provider usage fields", section 5.5.9, ADR-0005, ADR-0007).
//!
//! **Request.** Top-level `system` (omitted when empty), `messages` with
//! alternating roles (consecutive messages of one role are merged, empty
//! text blocks are left out), `max_tokens` (the applied output limit),
//! `stream`, `metadata.user_id` (the provider `user` value), `temperature`,
//! `top_p` and `stop_sequences` when set. No `extra_body`, no `user`, no
//! `max_tool_calls`. Images are sent as file reference blocks
//! (`{"type": "image", "source": {"type": "file", "file_id": ...}}`) with the
//! file id of the image's secondary copy in the Anthropic Files API
//! ([`LlmRequest::secondary_file_ids`]); an image without a copy is dropped.
//! Tools: `file_search` is never sent; `web_search` becomes the
//! `web_search_20250305` server tool, `code_interpreter` the
//! `code_execution_20250522` server tool, and function tools are sent as
//! client tools (`input_schema`). The `anthropic-version` header is always
//! sent; `anthropic-beta` names the Files API when an image block is sent and
//! code execution when that tool is. Function calls of an earlier agentic
//! iteration are sent as `tool_use` blocks of the assistant message and their
//! outputs as `tool_result` blocks of the following user message.
//!
//! **Stream** (`event:` names, falling back to the data's `type`):
//!
//! | Wire | Internal event |
//! |---|---|
//! | `message_start` | response id, input and cache usage |
//! | `content_block_start` `server_tool_use` `web_search` / `code_execution` (also `bash_code_execution`, `text_editor_code_execution`) | `tool` start `web_search` / `code_interpreter`, `details: {}` |
//! | `content_block_stop` of that block | `tool` done, `details: {}` |
//! | `content_block_start` `tool_use` | `tool` start named `search_knowledge`, `load_files` or `unknown_tool` (no done event) |
//! | `content_block_stop` of a `tool_use` block | the function call (arguments from the `input_json_delta`s) |
//! | `content_block_delta` `text_delta` | text delta |
//! | `content_block_delta` `citations_delta` (`web_search_result_location`) | web citation |
//! | `message_delta` | stop reason, output (and updated input/cache) usage |
//! | `message_stop`, or the end of the body after a stop reason | completed; `stop_reason = max_tokens` / `model_context_window_exceeded` is incomplete (`max_tokens`), `refusal` is incomplete (`content_filter`), `pause_turn` is incomplete (`other`) |
//! | `error` | `provider_error` with the sanitized provider message |
//!
//! Usage: Anthropic reports uncached input in `input_tokens`; the canonical
//! input is `input_tokens + cache_read_input_tokens +
//! cache_creation_input_tokens`, with the cache counters kept as subsets
//! (`cache_read_input_tokens`, `cache_write_input_tokens`). HTTP and gateway
//! errors are mapped like the `OpenAI` Responses adapter.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::ModelApiParams;
use oagw_sdk::api::ServiceGatewayClientV1;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::openai_responses::{
    error_fields, sse_error_failure, string_field, tool_completed, tool_started,
};
use super::sse::{STREAM_ENDED_MESSAGE, SseEvent, SseTranslator, event_stream};
use super::transport::{
    CANCELLED_MESSAGE, COMPLETION_BODY_LIMIT, INVALID_RESPONSE_MESSAGE, ProviderTransport,
    read_limited,
};
use crate::domain::llm::{
    LlmAnnotation, LlmAnnotationKind, LlmCompletion, LlmEvent, LlmEventStream, LlmFailure,
    LlmFunctionCall, LlmInputItem, LlmProvider, LlmRequest, LlmTool, LlmUsage, ProviderTarget,
    TOOL_CODE_INTERPRETER, TOOL_WEB_SEARCH,
};
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::oagw_provisioning::SystemContextSource;

/// `anthropic-version` header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// `anthropic-version` header name.
pub const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";
/// `anthropic-beta` header name.
pub const ANTHROPIC_BETA_HEADER: &str = "anthropic-beta";
/// Beta of file reference blocks and the Files API.
pub const FILES_API_BETA: &str = "files-api-2025-04-14";
/// Beta of the code execution server tool.
pub const CODE_EXECUTION_BETA: &str = "code-execution-2025-05-22";

/// Server tool type of `web_search`.
pub const WEB_SEARCH_TOOL_TYPE: &str = "web_search_20250305";
/// Server tool type of `code_interpreter`.
pub const CODE_EXECUTION_TOOL_TYPE: &str = "code_execution_20250522";
/// Server tool name of `code_interpreter`.
pub const CODE_EXECUTION_TOOL_NAME: &str = "code_execution";

/// `event: tool` names of client tool uses (DESIGN `event: tool`).
pub const TOOL_SEARCH_KNOWLEDGE: &str = "search_knowledge";
/// `event: tool` name of a `load_files` tool use.
pub const TOOL_LOAD_FILES: &str = "load_files";
/// `event: tool` name of any other client tool use.
pub const TOOL_UNKNOWN: &str = "unknown_tool";

/// Incomplete reason of a response cut at the output limit.
pub const INCOMPLETE_MAX_TOKENS: &str = "max_tokens";
/// Incomplete reason of a refused response.
pub const INCOMPLETE_CONTENT_FILTER: &str = "content_filter";
/// Incomplete reason of any other unfinished response.
pub const INCOMPLETE_OTHER: &str = "other";

/// The `anthropic_messages` adapter.
#[derive(Debug)]
pub struct AnthropicMessagesAdapter {
    transport: ProviderTransport,
}

impl AnthropicMessagesAdapter {
    /// Adapter over the OAGW client and the S2S context source.
    #[must_use]
    pub fn new(
        gateway: Arc<dyn ServiceGatewayClientV1>,
        system: Arc<dyn SystemContextSource>,
    ) -> Self {
        Self {
            transport: ProviderTransport::new(gateway, system),
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicMessagesAdapter {
    async fn stream(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmEventStream, LlmFailure> {
        let body = build_request_body(&req, true);
        let headers = request_headers(&body);
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LlmFailure::provider_error(CANCELLED_MESSAGE)),
            response = self.transport.send(target, &req.model, &body, true, &headers) => response?,
        };
        let body = response.into_body().into_stream();
        Ok(event_stream(body, MessagesTranslator::default(), cancel))
    }

    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, LlmFailure> {
        let body = build_request_body(&req, false);
        let headers = request_headers(&body);
        let response = self
            .transport
            .send(target, &req.model, &body, false, &headers)
            .await?;
        let body = read_limited(response.into_body(), COMPLETION_BODY_LIMIT).await;
        parse_completion(&body)
    }
}

// ── Request ──────────────────────────────────────────────────────────────

/// Messages API request body (module docs).
#[must_use]
pub fn build_request_body(req: &LlmRequest, stream: bool) -> Value {
    let mut body = Map::new();
    insert_api_params(&mut body, &req.api_params);
    body.insert("model".to_owned(), json!(req.model));
    body.insert("max_tokens".to_owned(), json!(req.max_output_tokens));
    body.insert("stream".to_owned(), json!(stream));
    if !req.instructions.is_empty() {
        body.insert("system".to_owned(), json!(req.instructions));
    }
    body.insert("messages".to_owned(), Value::Array(messages(req)));
    body.insert("metadata".to_owned(), json!({ "user_id": req.user }));
    let tools: Vec<Value> = req.tools.iter().filter_map(tool_json).collect();
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    Value::Object(body)
}

/// The `anthropic-version` and `anthropic-beta` headers of `body`.
#[must_use]
pub fn request_headers(body: &Value) -> Vec<(&'static str, String)> {
    let mut betas = Vec::new();
    let has_file_blocks = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .any(|block| block.pointer("/source/type").and_then(Value::as_str) == Some("file"));
    if has_file_blocks {
        betas.push(FILES_API_BETA);
    }
    let has_code_execution = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|tool| tool.get("type").and_then(Value::as_str) == Some(CODE_EXECUTION_TOOL_TYPE));
    if has_code_execution {
        betas.push(CODE_EXECUTION_BETA);
    }
    let mut headers = vec![(ANTHROPIC_VERSION_HEADER, ANTHROPIC_VERSION.to_owned())];
    if !betas.is_empty() {
        headers.push((ANTHROPIC_BETA_HEADER, betas.join(",")));
    }
    headers
}

fn insert_api_params(body: &mut Map<String, Value>, params: &ModelApiParams) {
    for (key, value) in [("temperature", params.temperature), ("top_p", params.top_p)] {
        if let Some(value) = value {
            body.insert(key.to_owned(), json!(value));
        }
    }
    if !params.stop.is_empty() {
        body.insert("stop_sequences".to_owned(), json!(params.stop));
    }
}

/// Conversation with alternating roles (module docs).
fn messages(req: &LlmRequest) -> Vec<Value> {
    let mut merged: Vec<(&'static str, Vec<Value>)> = Vec::new();
    for item in &req.input {
        let (role, blocks) = match item {
            LlmInputItem::User {
                text,
                image_file_ids,
            } => ("user", user_blocks(text, image_file_ids, req)),
            LlmInputItem::Assistant { text } => {
                ("assistant", text_block(text).into_iter().collect())
            }
            LlmInputItem::FunctionCall(call) => ("assistant", vec![tool_use_block(call)]),
            LlmInputItem::FunctionCallOutput { call_id, output } => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": output,
                })],
            ),
        };
        if blocks.is_empty() {
            continue;
        }
        match merged.last_mut() {
            Some((last_role, last_blocks)) if *last_role == role => last_blocks.extend(blocks),
            _ => merged.push((role, blocks)),
        }
    }
    merged
        .into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect()
}

/// The `tool_use` block of an earlier function call; arguments that are not
/// a JSON object are sent as an empty input.
fn tool_use_block(call: &LlmFunctionCall) -> Value {
    let input = serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    json!({
        "type": "tool_use",
        "id": call.call_id,
        "name": call.name,
        "input": input,
    })
}

fn text_block(text: &str) -> Option<Value> {
    (!text.is_empty()).then(|| json!({ "type": "text", "text": text }))
}

/// Image file reference blocks (secondary copies) followed by the text.
fn user_blocks(text: &str, image_file_ids: &[String], req: &LlmRequest) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::with_capacity(image_file_ids.len() + 1);
    for file_id in image_file_ids {
        if let Some(secondary) = req.secondary_file_ids.get(file_id) {
            blocks.push(json!({
                "type": "image",
                "source": { "type": "file", "file_id": secondary },
            }));
        } else {
            debug!(
                target: "mini_chat.llm",
                "image without an Anthropic Files API copy dropped from the request"
            );
        }
    }
    blocks.extend(text_block(text));
    blocks
}

fn tool_json(tool: &LlmTool) -> Option<Value> {
    match tool {
        LlmTool::FileSearch { .. } => None,
        LlmTool::WebSearch { .. } => Some(json!({
            "type": WEB_SEARCH_TOOL_TYPE,
            "name": TOOL_WEB_SEARCH,
        })),
        LlmTool::CodeInterpreter { .. } => Some(json!({
            "type": CODE_EXECUTION_TOOL_TYPE,
            "name": CODE_EXECUTION_TOOL_NAME,
        })),
        LlmTool::Function(function) => Some(json!({
            "name": function.name,
            "description": function.description,
            "input_schema": function.parameters,
        })),
    }
}

// ── Usage, stop reasons, completion ──────────────────────────────────────

/// Anthropic usage counters as reported (input is uncached input only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WireUsage {
    input: i64,
    output: i64,
    cache_read: i64,
    cache_creation: i64,
}

impl WireUsage {
    /// Updates the counters present in `usage`.
    fn update(&mut self, usage: &Value) {
        let fields = [
            ("input_tokens", &mut self.input),
            ("output_tokens", &mut self.output),
            ("cache_read_input_tokens", &mut self.cache_read),
            ("cache_creation_input_tokens", &mut self.cache_creation),
        ];
        for (key, counter) in fields {
            if let Some(value) = usage.get(key).and_then(Value::as_i64) {
                *counter = value;
            }
        }
    }

    /// Canonical usage: the cache counters are subsets of the input.
    const fn normalized(self) -> LlmUsage {
        LlmUsage {
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

/// Canonical usage of an Anthropic `usage` object.
#[must_use]
pub fn normalize_usage(usage: &Value) -> LlmUsage {
    let mut wire = WireUsage::default();
    wire.update(usage);
    wire.normalized()
}

/// Incomplete reason of a `stop_reason`; `None` for a complete response.
#[must_use]
pub fn incomplete_reason(stop_reason: &str) -> Option<String> {
    match stop_reason {
        "max_tokens" | "model_context_window_exceeded" => Some(INCOMPLETE_MAX_TOKENS.to_owned()),
        "refusal" => Some(INCOMPLETE_CONTENT_FILTER.to_owned()),
        "pause_turn" => Some(INCOMPLETE_OTHER.to_owned()),
        _ => None,
    }
}

/// Result of a non-streaming response body.
///
/// # Errors
///
/// `provider_error` for a body that is not a message or is an error.
pub fn parse_completion(body: &[u8]) -> Result<LlmCompletion, LlmFailure> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| LlmFailure::provider_error(INVALID_RESPONSE_MESSAGE))?;
    if value.get("type").and_then(Value::as_str) == Some("error")
        || value.get("error").is_some_and(|error| !error.is_null())
    {
        let (message, code) =
            error_fields(&value).unwrap_or_else(|| (INVALID_RESPONSE_MESSAGE.to_owned(), None));
        return Err(LlmFailure {
            provider_code: code,
            ..LlmFailure::provider_error(sanitize_provider_message(&message))
        });
    }
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmFailure::provider_error(INVALID_RESPONSE_MESSAGE))?;
    let text = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    let stop_reason = value
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(LlmCompletion {
        text,
        usage: value
            .get("usage")
            .filter(|usage| usage.is_object())
            .map(normalize_usage),
        response_id: string_field(&value, "id"),
        incomplete_reason: incomplete_reason(stop_reason),
    })
}

// ── Stream translation ───────────────────────────────────────────────────

/// An open content block.
#[derive(Debug)]
enum Block {
    /// A server tool call reported under the given tool name.
    ServerTool(&'static str),
    /// A client tool use.
    ToolUse {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Text and any other block.
    Other,
}

/// Messages API stream translation (module docs).
#[derive(Debug, Default)]
pub struct MessagesTranslator {
    response_id: Option<String>,
    usage: Option<WireUsage>,
    stop_reason: Option<String>,
    text: String,
    blocks: HashMap<u64, Block>,
    seen_citations: HashSet<String>,
}

/// Event name from the `event:` line, or the data's `type`.
fn event_name(event: Option<&str>, data: Option<&Value>) -> String {
    match event {
        Some(name) if !name.is_empty() && name != "message" => name.to_owned(),
        _ => data
            .and_then(|data| data.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    }
}

/// Shared tool name of a server tool, `None` for tools the gear does not
/// count.
fn server_tool_name(name: &str) -> Option<&'static str> {
    match name {
        "web_search" => Some(TOOL_WEB_SEARCH),
        "code_execution" | "bash_code_execution" | "text_editor_code_execution" => {
            Some(TOOL_CODE_INTERPRETER)
        }
        _ => None,
    }
}

/// `event: tool` name of a client tool use.
fn client_tool_event_name(name: &str) -> &'static str {
    match name {
        TOOL_SEARCH_KNOWLEDGE => TOOL_SEARCH_KNOWLEDGE,
        TOOL_LOAD_FILES => TOOL_LOAD_FILES,
        _ => TOOL_UNKNOWN,
    }
}

fn block_index(data: &Value) -> u64 {
    data.get("index").and_then(Value::as_u64).unwrap_or(0)
}

impl SseTranslator for MessagesTranslator {
    fn translate(&mut self, event: SseEvent, out: &mut Vec<LlmEvent>) {
        let data: Option<Value> = serde_json::from_str(&event.data).ok();
        let name = event_name(event.event.as_deref(), data.as_ref());
        if name == "error" {
            out.push(LlmEvent::Failed(sse_error_failure(
                data.as_ref(),
                &event.data,
            )));
            return;
        }
        let Some(data) = data else {
            return;
        };
        match name.as_str() {
            "message_start" => self.on_message_start(&data),
            "content_block_start" => self.on_block_start(&data, out),
            "content_block_delta" => self.on_block_delta(&data, out),
            "content_block_stop" => self.on_block_stop(&data, out),
            "message_delta" => self.on_message_delta(&data),
            "message_stop" => out.push(self.completed()),
            _ => {}
        }
    }

    fn stream_ended(&mut self) -> LlmEvent {
        if self.stop_reason.is_some() {
            self.completed()
        } else {
            LlmEvent::Failed(LlmFailure::provider_error(STREAM_ENDED_MESSAGE))
        }
    }
}

impl MessagesTranslator {
    fn on_message_start(&mut self, data: &Value) {
        let Some(message) = data.get("message") else {
            return;
        };
        self.response_id = string_field(message, "id");
        if let Some(usage) = message.get("usage").filter(|usage| usage.is_object()) {
            self.usage
                .get_or_insert_with(WireUsage::default)
                .update(usage);
        }
    }

    fn on_block_start(&mut self, data: &Value, out: &mut Vec<LlmEvent>) {
        let index = block_index(data);
        let Some(block) = data.get("content_block") else {
            return;
        };
        let name = string_field(block, "name").unwrap_or_default();
        let opened = match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    self.push_text(text, out);
                }
                Block::Other
            }
            Some("server_tool_use") => match server_tool_name(&name) {
                Some(tool) => {
                    out.push(tool_started(tool));
                    Block::ServerTool(tool)
                }
                None => Block::Other,
            },
            Some("tool_use") => {
                out.push(tool_started(client_tool_event_name(&name)));
                Block::ToolUse {
                    call_id: string_field(block, "id").unwrap_or_default(),
                    name,
                    arguments: String::new(),
                }
            }
            _ => Block::Other,
        };
        self.blocks.insert(index, opened);
    }

    fn on_block_delta(&mut self, data: &Value, out: &mut Vec<LlmEvent>) {
        let Some(delta) = data.get("delta") else {
            return;
        };
        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    self.push_text(text, out);
                }
            }
            Some("input_json_delta") => {
                if let (Some(Block::ToolUse { arguments, .. }), Some(partial)) = (
                    self.blocks.get_mut(&block_index(data)),
                    delta.get("partial_json").and_then(Value::as_str),
                ) {
                    arguments.push_str(partial);
                }
            }
            Some("citations_delta") => {
                if let Some(citation) = delta.get("citation") {
                    self.on_citation(citation, out);
                }
            }
            _ => {}
        }
    }

    fn on_block_stop(&mut self, data: &Value, out: &mut Vec<LlmEvent>) {
        match self.blocks.remove(&block_index(data)) {
            Some(Block::ServerTool(tool)) => out.push(tool_completed(tool, json!({}))),
            Some(Block::ToolUse {
                call_id,
                name,
                arguments,
            }) => out.push(LlmEvent::FunctionCall(LlmFunctionCall {
                call_id,
                name,
                arguments: if arguments.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    arguments
                },
            })),
            Some(Block::Other) | None => {}
        }
    }

    fn on_message_delta(&mut self, data: &Value) {
        if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
            self.stop_reason = Some(reason.to_owned());
        }
        if let Some(usage) = data.get("usage").filter(|usage| usage.is_object()) {
            self.usage
                .get_or_insert_with(WireUsage::default)
                .update(usage);
        }
    }

    fn on_citation(&mut self, citation: &Value, out: &mut Vec<LlmEvent>) {
        if citation.get("type").and_then(Value::as_str) != Some("web_search_result_location") {
            return;
        }
        let Some(url) = string_field(citation, "url") else {
            return;
        };
        let snippet = string_field(citation, "cited_text").unwrap_or_default();
        if !self.seen_citations.insert(format!("{url}|{snippet}")) {
            return;
        }
        out.push(LlmEvent::Annotation(LlmAnnotation {
            kind: LlmAnnotationKind::Url,
            url: Some(url),
            title: string_field(citation, "title"),
            file_id: None,
            filename: None,
            start: None,
            end: None,
            snippet,
        }));
    }

    fn push_text(&mut self, text: &str, out: &mut Vec<LlmEvent>) {
        if text.is_empty() {
            return;
        }
        self.text.push_str(text);
        out.push(LlmEvent::TextDelta(text.to_owned()));
    }

    fn completed(&mut self) -> LlmEvent {
        LlmEvent::Completed {
            response_id: self.response_id.take(),
            usage: self.usage.map(WireUsage::normalized),
            incomplete_reason: self.stop_reason.as_deref().and_then(incomplete_reason),
            output_text: std::mem::take(&mut self.text),
        }
    }
}

#[cfg(test)]
#[path = "anthropic_messages_tests.rs"]
mod tests;
