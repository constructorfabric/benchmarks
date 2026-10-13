//! `openai_responses` adapter: `OpenAI` and Azure `OpenAI` Responses API (DESIGN
//! section 3.2 `llm_provider`, section 3.5 "LLM Provider", "Provider Event
//! Translation", "Provider Request Metadata", ADR-0005).
//!
//! The adapter builds the Responses API request body, sends it through the
//! in-process OAGW client to `/{alias}{api_path}` (the `{model}` placeholder
//! replaced by the provider model id) with the gear's S2S security context
//! (the upstreams are registered under that context, see
//! [`crate::infra::oagw_provisioning`]), and translates the provider SSE
//! stream into [`LlmEvent`]s. Error text that can reach clients is sanitized.
//!
//! The request body, the stream translation and the error mapping are shared
//! with the `vllm_responses` adapter ([`super::vllm_responses`]), which sends
//! a reduced body and splits `<think>` blocks out of the text.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, StatusCode, header};
use mini_chat_sdk::ModelApiParams;
use oagw_sdk::ServiceGatewayError;
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use tracing::warn;

use super::sse::{SseEvent, SseTranslator, event_stream};
use super::transport::{
    CANCELLED_MESSAGE, COMPLETION_BODY_LIMIT, INVALID_RESPONSE_MESSAGE, ProviderTransport,
    read_limited,
};
use super::vllm_responses::{ThinkSegment, ThinkSplitter, strip_think};
use crate::domain::llm::{
    LlmAnnotation, LlmAnnotationKind, LlmCompletion, LlmEvent, LlmEventStream, LlmFailure,
    LlmFailureKind, LlmFunctionCall, LlmInputItem, LlmProvider, LlmRequest, LlmTool, LlmUsage,
    ProviderTarget, TOOL_CODE_INTERPRETER, TOOL_FILE_SEARCH, TOOL_WEB_SEARCH,
};
use crate::domain::sanitize::sanitize_provider_message;
use crate::infra::oagw_provisioning::SystemContextSource;

/// Top-level request keys the request controls; `extra_body` entries with
/// these keys are ignored with a warning (DESIGN model catalog
/// `general_config.api_params`).
pub const CONTROLLED_KEYS: &[&str] = &[
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

/// Character cap of the code interpreter `output` tool detail.
pub const CODE_OUTPUT_MAX_CHARS: usize = 8192;
/// Suffix appended to a truncated code interpreter output.
pub const TRUNCATED_SUFFIX: &str = "...[truncated]";

const CODE_INTERPRETER_INCLUDE: &str = "code_interpreter_call.outputs";

const RATE_LIMITED_MESSAGE: &str = "Provider rate limit exceeded";
const TIMEOUT_MESSAGE: &str = "Provider request timed out";
const UNAVAILABLE_MESSAGE: &str = "Provider is currently unavailable";
const GATEWAY_FAILED_MESSAGE: &str = "Provider request failed";
const FAILED_RESPONSE_MESSAGE: &str = "Provider response failed";
const SSE_ERROR_MESSAGE: &str = "Provider stream error";

/// The `openai_responses` adapter.
pub struct OpenAiResponsesAdapter {
    transport: ProviderTransport,
}

impl std::fmt::Debug for OpenAiResponsesAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiResponsesAdapter")
            .finish_non_exhaustive()
    }
}

impl OpenAiResponsesAdapter {
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
impl LlmProvider for OpenAiResponsesAdapter {
    async fn stream(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmEventStream, LlmFailure> {
        let body = build_request_body(&req, true);
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(LlmFailure::provider_error(CANCELLED_MESSAGE)),
            response = self.transport.send(target, &req.model, &body, true, &[]) => response?,
        };
        let body = response.into_body().into_stream();
        Ok(event_stream(body, ResponsesTranslator::default(), cancel))
    }

    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, LlmFailure> {
        let body = build_request_body(&req, false);
        let response = self
            .transport
            .send(target, &req.model, &body, false, &[])
            .await?;
        let body = read_limited(response.into_body(), COMPLETION_BODY_LIMIT).await;
        parse_completion(&body)
    }
}

// ── Request ──────────────────────────────────────────────────────────────

/// Responses API request body.
#[must_use]
pub fn build_request_body(req: &LlmRequest, stream: bool) -> Value {
    let mut body = Map::new();
    merge_extra_body(&mut body, req.api_params.extra_body.as_ref());
    insert_api_params(&mut body, &req.api_params);

    body.insert("model".to_owned(), json!(req.model));
    if !req.instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(req.instructions));
    }
    let input: Vec<Value> = req.input.iter().map(input_item).collect();
    body.insert("input".to_owned(), Value::Array(input));
    body.insert("stream".to_owned(), json!(stream));
    body.insert("max_output_tokens".to_owned(), json!(req.max_output_tokens));
    body.insert("max_tool_calls".to_owned(), json!(req.max_tool_calls));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req.tools.iter().map(tool_json).collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    if req
        .tools
        .iter()
        .any(|tool| matches!(tool, LlmTool::CodeInterpreter { .. }))
    {
        body.insert("include".to_owned(), json!([CODE_INTERPRETER_INCLUDE]));
    }
    body.insert("user".to_owned(), json!(req.user));
    body.insert(
        "metadata".to_owned(),
        json!({
            "tenant_id": req.metadata.tenant_id,
            "user_id": req.metadata.user_id,
            "chat_id": req.metadata.chat_id,
            "request_type": req.metadata.request_type.as_str(),
            "feature": req.metadata.feature,
        }),
    );
    Value::Object(body)
}

/// Merges `extra_body` into the top level of `body`, skipping (with a
/// warning) the keys in [`CONTROLLED_KEYS`].
pub(crate) fn merge_extra_body(body: &mut Map<String, Value>, extra: Option<&Map<String, Value>>) {
    for (key, value) in extra.into_iter().flatten() {
        if CONTROLLED_KEYS.contains(&key.as_str()) {
            warn!(target: "mini_chat.llm", key = %key, "extra_body key is controlled by the request and is ignored");
            continue;
        }
        body.insert(key.clone(), value.clone());
    }
}

/// Sampling parameters, `stop` and `reasoning.effort` of the Responses
/// protocol (each only when set).
pub(crate) fn insert_api_params(body: &mut Map<String, Value>, params: &ModelApiParams) {
    let sampling = [
        ("temperature", params.temperature),
        ("top_p", params.top_p),
        ("frequency_penalty", params.frequency_penalty),
        ("presence_penalty", params.presence_penalty),
    ];
    for (key, value) in sampling {
        if let Some(value) = value {
            body.insert(key.to_owned(), json!(value));
        }
    }
    if !params.stop.is_empty() {
        body.insert("stop".to_owned(), json!(params.stop));
    }
    if let Some(effort) = params.reasoning_effort.as_deref() {
        body.insert("reasoning".to_owned(), json!({ "effort": effort }));
    }
}

/// One Responses `input` item.
pub(crate) fn input_item(item: &LlmInputItem) -> Value {
    match item {
        LlmInputItem::User {
            text,
            image_file_ids,
        } => {
            let mut content = vec![json!({ "type": "input_text", "text": text })];
            content.extend(
                image_file_ids
                    .iter()
                    .map(|file_id| json!({ "type": "input_image", "file_id": file_id })),
            );
            json!({ "role": "user", "content": content })
        }
        LlmInputItem::Assistant { text } => json!({
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        }),
        LlmInputItem::FunctionCall(call) => json!({
            "type": "function_call",
            "call_id": call.call_id,
            "name": call.name,
            "arguments": call.arguments,
        }),
        LlmInputItem::FunctionCallOutput { call_id, output } => json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        }),
    }
}

fn tool_json(tool: &LlmTool) -> Value {
    match tool {
        LlmTool::FileSearch {
            vector_store_id,
            max_num_results,
        } => json!({
            "type": TOOL_FILE_SEARCH,
            "vector_store_ids": [vector_store_id],
            "max_num_results": max_num_results,
        }),
        LlmTool::WebSearch {
            search_context_size,
        } => json!({ "type": TOOL_WEB_SEARCH, "search_context_size": search_context_size }),
        LlmTool::CodeInterpreter { file_ids } => json!({
            "type": TOOL_CODE_INTERPRETER,
            "container": { "type": "auto", "file_ids": file_ids },
        }),
        LlmTool::Function(function) => json!({
            "type": "function",
            "name": function.name,
            "description": function.description,
            "parameters": function.parameters,
        }),
    }
}

/// OAGW proxy URI `/{alias}{api_path}` with `{model}` replaced by the
/// percent-encoded model id.
#[must_use]
pub fn request_uri(alias: &str, api_path: &str, model: &str) -> String {
    let path = api_path.replace("{model}", &encode_path_segment(model));
    format!("/{alias}{path}")
}

pub(crate) fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':' | b'@') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
    encoded
}

// ── Errors ───────────────────────────────────────────────────────────────

/// Failure of a gateway-level error (`proxy_request` returned `Err`).
#[must_use]
pub fn map_gateway_error(provider_id: &str, err: CanonicalError) -> LlmFailure {
    match ServiceGatewayError::from(err) {
        ServiceGatewayError::Timeout => {
            LlmFailure::new(LlmFailureKind::ProviderTimeout, TIMEOUT_MESSAGE)
        }
        ServiceGatewayError::RateLimited { retry_after_secs } => rate_limited(retry_after_secs),
        ServiceGatewayError::Unavailable { .. } => {
            warn!(target: "mini_chat.llm", provider = %provider_id, "provider upstream unavailable");
            LlmFailure::provider_error(UNAVAILABLE_MESSAGE)
        }
        other => {
            warn!(target: "mini_chat.llm", provider = %provider_id, error = %other, "provider request rejected by the gateway");
            LlmFailure::provider_error(GATEWAY_FAILED_MESSAGE)
        }
    }
}

/// Failure of a non-2xx response: 429 is `rate_limited` (with a numeric
/// `Retry-After` in the message); a gateway 504 (`deadline_exceeded`) is
/// `provider_timeout`; everything else, including a provider's own 504, is
/// `provider_error` with the sanitized provider message.
#[must_use]
pub fn map_http_error(
    status: StatusCode,
    headers: &HeaderMap,
    source: Option<ErrorSource>,
    body: &[u8],
) -> LlmFailure {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return rate_limited(retry_after(headers));
    }
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    if status == StatusCode::GATEWAY_TIMEOUT
        && (source == Some(ErrorSource::Gateway) || is_deadline_problem(parsed.as_ref()))
    {
        return LlmFailure::new(LlmFailureKind::ProviderTimeout, TIMEOUT_MESSAGE);
    }
    let (message, code) = parsed
        .as_ref()
        .and_then(error_fields)
        .unwrap_or_else(|| (format!("Provider returned HTTP {}", status.as_u16()), None));
    LlmFailure {
        provider_code: code,
        ..LlmFailure::provider_error(sanitize_provider_message(&message))
    }
}

pub(crate) fn rate_limited(retry_after_secs: Option<u64>) -> LlmFailure {
    let message = match retry_after_secs {
        Some(secs) => format!("{RATE_LIMITED_MESSAGE}; retry after {secs} seconds"),
        None => RATE_LIMITED_MESSAGE.to_owned(),
    };
    LlmFailure {
        retry_after_secs,
        ..LlmFailure::new(LlmFailureKind::RateLimited, message)
    }
}

fn retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn is_deadline_problem(body: Option<&Value>) -> bool {
    body.and_then(|value| value.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|problem_type| problem_type.contains("deadline_exceeded"))
}

/// `(message, code)` of a provider or gateway error body: `error.message`
/// (`error.code` / `error.type`), a string `error`, a flat `message`
/// (`code`), or a Problem `detail` / `title`.
pub(crate) fn error_fields(body: &Value) -> Option<(String, Option<String>)> {
    let (message, code) = match body.get("error") {
        Some(Value::Object(error)) => (
            error.get("message").and_then(Value::as_str),
            code_of(error.get("code")).or_else(|| code_of(error.get("type"))),
        ),
        Some(Value::String(message)) => (Some(message.as_str()), None),
        _ => (
            body.get("message")
                .or_else(|| body.get("detail"))
                .or_else(|| body.get("title"))
                .and_then(Value::as_str),
            code_of(body.get("code")),
        ),
    };
    let message = message.filter(|message| !message.trim().is_empty())?;
    Some((message.to_owned(), code))
}

fn code_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(code) => Some(code.clone()),
        Value::Number(code) => Some(code.to_string()),
        _ => None,
    }
}

/// `provider_error` of a `failed` response: the error is read from
/// `response.error`, with the envelope's top-level `error` as fallback;
/// `response.usage` is kept.
fn failed_response_failure(response: Option<&Value>, envelope: &Value) -> LlmFailure {
    let error = response
        .and_then(|response| response.get("error"))
        .filter(|error| error.is_object())
        .or_else(|| envelope.get("error").filter(|error| !error.is_null()));
    let (message, code) = error
        .and_then(|error| error_fields(&json!({ "error": error })))
        .unwrap_or_else(|| (FAILED_RESPONSE_MESSAGE.to_owned(), None));
    LlmFailure {
        provider_code: code,
        usage: response.and_then(parse_usage),
        ..LlmFailure::provider_error(sanitize_provider_message(&message))
    }
}

/// SSE `error` event: parsed like `response.failed`, then as flat
/// `{code, message}`; unparseable data becomes the message.
pub(crate) fn sse_error_failure(data: Option<&Value>, raw: &str) -> LlmFailure {
    if let Some(value) = data {
        let nested = value.get("response").is_some()
            || value.get("error").is_some_and(|error| !error.is_null());
        if nested {
            return failed_response_failure(value.get("response"), value);
        }
        if let Some((message, code)) = error_fields(value) {
            return LlmFailure {
                provider_code: code,
                ..LlmFailure::provider_error(sanitize_provider_message(&message))
            };
        }
    }
    let message = if raw.trim().is_empty() || data.is_some() {
        SSE_ERROR_MESSAGE
    } else {
        raw
    };
    LlmFailure::provider_error(sanitize_provider_message(message))
}

// ── Responses ────────────────────────────────────────────────────────────

/// Result of a non-streaming response body.
///
/// # Errors
///
/// `provider_error` for a body that is not a response object or a `failed`
/// response.
pub fn parse_completion(body: &[u8]) -> Result<LlmCompletion, LlmFailure> {
    let response: Value = serde_json::from_slice(body)
        .map_err(|_| LlmFailure::provider_error(INVALID_RESPONSE_MESSAGE))?;
    let status = response.get("status").and_then(Value::as_str);
    if status == Some("failed") {
        return Err(failed_response_failure(Some(&response), &response));
    }
    Ok(LlmCompletion {
        text: response_output_text(&response).unwrap_or_default(),
        usage: parse_usage(&response),
        response_id: string_field(&response, "id"),
        incomplete_reason: (status == Some("incomplete")).then(|| incomplete_reason(&response)),
    })
}

pub(crate) fn parse_usage(response: &Value) -> Option<LlmUsage> {
    let usage = response.get("usage").filter(|usage| usage.is_object())?;
    let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_i64).unwrap_or(0);
    Some(LlmUsage {
        input_tokens: count("/input_tokens"),
        output_tokens: count("/output_tokens"),
        cache_read_input_tokens: count("/input_tokens_details/cached_tokens"),
        cache_write_input_tokens: 0,
        reasoning_tokens: count("/output_tokens_details/reasoning_tokens"),
    })
}

fn incomplete_reason(response: &Value) -> String {
    response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned()
}

pub(crate) fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// `output_text` parts of the response's message items: `(text, annotations)`.
fn output_text_parts(response: &Value) -> Vec<(&str, &[Value])> {
    let items = response.get("output").and_then(Value::as_array);
    items
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .map(|part| {
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            let annotations = part
                .get("annotations")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            (text, annotations)
        })
        .collect()
}

/// Concatenated output text; `None` when the response has no text part.
fn response_output_text(response: &Value) -> Option<String> {
    let parts = output_text_parts(response);
    if parts.is_empty() {
        return None;
    }
    Some(parts.into_iter().map(|(text, _)| text).collect())
}

/// Code interpreter `logs` outputs joined with `\n`, see [`truncate_output`].
fn code_interpreter_output(item: &Value) -> String {
    let logs: Vec<&str> = item
        .get("outputs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|output| output.get("type").and_then(Value::as_str) == Some("logs"))
        .filter_map(|output| output.get("logs").and_then(Value::as_str))
        .collect();
    truncate_output(&logs.join("\n"))
}

/// Cap at [`CODE_OUTPUT_MAX_CHARS`] characters, appending
/// [`TRUNCATED_SUFFIX`] when cut.
#[must_use]
pub fn truncate_output(output: &str) -> String {
    match output.char_indices().nth(CODE_OUTPUT_MAX_CHARS) {
        Some((cut, _)) => format!("{}{TRUNCATED_SUFFIX}", &output[..cut]),
        None => output.to_owned(),
    }
}

// ── Annotations ──────────────────────────────────────────────────────────

pub(crate) fn parse_annotation(annotation: &Value, part_text: &str) -> Option<LlmAnnotation> {
    let kind = match annotation.get("type").and_then(Value::as_str)? {
        "url_citation" => LlmAnnotationKind::Url,
        "file_citation" => LlmAnnotationKind::File,
        _ => return None,
    };
    let index = |key: &str| {
        annotation
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
    };
    let (start, end) = match (index("start_index"), index("end_index")) {
        (Some(start), Some(end)) => (Some(start), Some(end)),
        _ => (None, None),
    };
    let snippet = match kind {
        LlmAnnotationKind::File => String::new(),
        LlmAnnotationKind::Url => string_field(annotation, "text")
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| range_text(part_text, start, end)),
    };
    Some(LlmAnnotation {
        kind,
        url: string_field(annotation, "url"),
        title: string_field(annotation, "title"),
        file_id: string_field(annotation, "file_id"),
        filename: string_field(annotation, "filename"),
        start,
        end,
        snippet,
    })
}

/// Characters `start..end` of `text`; empty when the range is missing or
/// outside the text.
fn range_text(text: &str, start: Option<usize>, end: Option<usize>) -> String {
    match (start, end) {
        (Some(start), Some(end)) if start <= end && end <= text.chars().count() => {
            text.chars().skip(start).take(end - start).collect()
        }
        _ => String::new(),
    }
}

/// Identity of an annotation for deduplication between
/// `response.output_text.annotation.added` and the completed response.
fn annotation_key(annotation: &Value) -> String {
    [
        "type",
        "url",
        "file_id",
        "start_index",
        "end_index",
        "index",
    ]
    .iter()
    .map(|key| {
        annotation
            .get(*key)
            .map(Value::to_string)
            .unwrap_or_default()
    })
    .collect::<Vec<_>>()
    .join("|")
}

fn emit_annotation(
    seen: &mut HashSet<String>,
    annotation: &Value,
    part_text: &str,
    out: &mut Vec<LlmEvent>,
) {
    let Some(parsed) = parse_annotation(annotation, part_text) else {
        return;
    };
    if seen.insert(annotation_key(annotation)) {
        out.push(LlmEvent::Annotation(parsed));
    }
}

// ── Stream translation ───────────────────────────────────────────────────

/// Responses API stream translation (DESIGN "Provider Event Translation").
#[derive(Debug, Default)]
pub struct ResponsesTranslator {
    /// Text of every `(item_id, content_index)` part, for annotation ranges.
    parts: HashMap<(String, u64), String>,
    /// All text deltas, used when the completed response carries no text.
    text: String,
    seen_annotations: HashSet<String>,
    /// `vllm_responses`: `<think>` blocks of the text become reasoning.
    think: Option<ThinkSplitter>,
}

/// Event name from the `event:` line, or the data's `type` when the line is
/// missing or `message`.
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

pub(crate) fn tool_started(name: &str) -> LlmEvent {
    LlmEvent::ToolStarted {
        name: name.to_owned(),
        details: json!({}),
    }
}

pub(crate) fn tool_completed(name: &str, details: Value) -> LlmEvent {
    LlmEvent::ToolCompleted {
        name: name.to_owned(),
        details,
    }
}

fn part_key(data: &Value) -> (String, u64) {
    (
        string_field(data, "item_id").unwrap_or_default(),
        data.get("content_index")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
}

impl SseTranslator for ResponsesTranslator {
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
        if let Some(data) = data {
            self.translate_json(&name, &data, out);
        }
    }
}

impl ResponsesTranslator {
    /// Translation of the `vllm_responses` adapter: text inside `<think>`
    /// blocks (and `response.reasoning_text.delta`) is reasoning.
    #[must_use]
    pub fn with_think_split() -> Self {
        Self {
            think: Some(ThinkSplitter::default()),
            ..Self::default()
        }
    }

    fn translate_json(&mut self, name: &str, data: &Value, out: &mut Vec<LlmEvent>) {
        match name {
            "response.output_text.delta" => self.on_text_delta(data, out),
            "response.reasoning_text.delta" if self.think.is_some() => {
                if let Some(delta) = data.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(LlmEvent::ReasoningDelta(delta.to_owned()));
                }
            }
            "response.output_text.annotation.added" => self.on_annotation_added(data, out),
            "response.file_search_call.searching" => out.push(tool_started(TOOL_FILE_SEARCH)),
            "response.file_search_call.completed" => {
                let searched = data
                    .get("results")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                out.push(tool_completed(
                    TOOL_FILE_SEARCH,
                    json!({ "files_searched": searched }),
                ));
            }
            "response.web_search_call.searching" => out.push(tool_started(TOOL_WEB_SEARCH)),
            "response.web_search_call.completed" => {
                out.push(tool_completed(TOOL_WEB_SEARCH, json!({})));
            }
            "response.code_interpreter_call.in_progress" => {
                out.push(tool_started(TOOL_CODE_INTERPRETER));
            }
            "response.output_item.done" => on_output_item_done(data, out),
            "response.completed" => self.on_completed(data, false, out),
            "response.incomplete" => self.on_completed(data, true, out),
            "response.failed" => out.push(LlmEvent::Failed(failed_response_failure(
                data.get("response"),
                data,
            ))),
            _ => {}
        }
    }

    fn on_text_delta(&mut self, data: &Value, out: &mut Vec<LlmEvent>) {
        let Some(delta) = data.get("delta").and_then(Value::as_str) else {
            return;
        };
        if delta.is_empty() {
            return;
        }
        self.parts
            .entry(part_key(data))
            .or_default()
            .push_str(delta);
        match self.think.as_mut() {
            None => {
                self.text.push_str(delta);
                out.push(LlmEvent::TextDelta(delta.to_owned()));
            }
            Some(splitter) => {
                let segments = splitter.push(delta);
                self.emit_segments(segments, out);
            }
        }
    }

    fn emit_segments(&mut self, segments: Vec<ThinkSegment>, out: &mut Vec<LlmEvent>) {
        for segment in segments {
            match segment {
                ThinkSegment::Text(text) => {
                    self.text.push_str(&text);
                    out.push(LlmEvent::TextDelta(text));
                }
                ThinkSegment::Reasoning(text) => out.push(LlmEvent::ReasoningDelta(text)),
            }
        }
    }

    fn on_annotation_added(&mut self, data: &Value, out: &mut Vec<LlmEvent>) {
        let Some(annotation) = data.get("annotation") else {
            return;
        };
        let part_text = self.parts.get(&part_key(data)).map_or("", String::as_str);
        emit_annotation(&mut self.seen_annotations, annotation, part_text, out);
    }

    fn on_completed(&mut self, data: &Value, incomplete: bool, out: &mut Vec<LlmEvent>) {
        if let Some(splitter) = self.think.as_mut() {
            let segments = splitter.finish();
            self.emit_segments(segments, out);
        }
        let response = data.get("response").unwrap_or(&Value::Null);
        for (part_text, annotations) in output_text_parts(response) {
            for annotation in annotations {
                emit_annotation(&mut self.seen_annotations, annotation, part_text, out);
            }
        }
        let output_text = match (response_output_text(response), self.think.is_some()) {
            (Some(text), false) => text,
            (Some(text), true) => strip_think(&text),
            (None, _) => std::mem::take(&mut self.text),
        };
        out.push(LlmEvent::Completed {
            response_id: string_field(response, "id"),
            usage: parse_usage(response),
            incomplete_reason: incomplete.then(|| incomplete_reason(response)),
            output_text,
        });
    }
}

fn on_output_item_done(data: &Value, out: &mut Vec<LlmEvent>) {
    let Some(item) = data.get("item") else {
        return;
    };
    match item.get("type").and_then(Value::as_str) {
        Some("code_interpreter_call") => out.push(tool_completed(
            TOOL_CODE_INTERPRETER,
            json!({ "output": code_interpreter_output(item) }),
        )),
        // A client function tool: no client event (DESIGN `event: tool`),
        // only the internal tool-use signal.
        Some("function_call") => out.push(LlmEvent::FunctionCall(LlmFunctionCall {
            call_id: string_field(item, "call_id").unwrap_or_default(),
            name: string_field(item, "name").unwrap_or_default(),
            arguments: string_field(item, "arguments").unwrap_or_default(),
        })),
        _ => {}
    }
}

#[cfg(test)]
#[path = "openai_responses_tests.rs"]
mod tests;
