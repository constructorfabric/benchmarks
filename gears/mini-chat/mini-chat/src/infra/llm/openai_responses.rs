//! The `openai_responses` adapter (`OpenAI` and Azure `OpenAI` Responses API,
//! DESIGN 3.2 `llm_provider`, 3.3 "Provider Event Translation", 4 "Provider
//! Request Metadata"): request building, SSE event parsing and error mapping
//! over the in-process OAGW proxy client.

use bytes::Bytes;
use futures::StreamExt;
use http::header::{ACCEPT, CONTENT_TYPE, RETRY_AFTER};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::body::Body;
use oagw_sdk::{ServerEvent, ServerEventsResponse, ServerEventsStream, ServiceGatewayClientV1};
use serde_json::{Map, Value, json};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use crate::domain::context::HistoryRole;
pub use crate::domain::provider::{Annotation, Completion, Failure, ProviderEvent, TokenBreakdown};
use crate::domain::provider::{
    ChatRequest, ChatTool, ProviderErrorKind, ProviderEventStream, ProviderFailure,
};
use crate::domain::sanitize::client_message;

/// Longest `code_interpreter` output forwarded in a `tool` event, in characters.
const CODE_OUTPUT_MAX_CHARS: usize = 8192;

/// Suffix of a truncated `code_interpreter` output.
const TRUNCATED_SUFFIX: &str = "...[truncated]";

/// Largest provider error body read for the message.
const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// Largest non-streaming (summary) response body read.
const RESPONSE_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// Top-level request keys the gear controls; `api_params.extra_body` cannot
/// override them.
const CONTROLLED_KEYS: [&str; 18] = [
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

// ── event parsing ────────────────────────────────────────────────────────────

/// Translates one provider SSE event. The event name is the `event:` line;
/// when it is empty or `message` the `type` field of the data is used.
#[must_use]
pub fn parse_event(event_name: &str, data: &str) -> ProviderEvent {
    let parsed: Option<Value> = serde_json::from_str(data).ok();
    let name = if event_name.is_empty() || event_name == "message" {
        parsed
            .as_ref()
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
    } else {
        event_name
    };
    let value = parsed.as_ref();
    match name {
        "response.output_text.delta" => value
            .and_then(|v| v.get("delta"))
            .and_then(Value::as_str)
            .map_or(ProviderEvent::Ignored, |delta| {
                ProviderEvent::TextDelta(delta.to_owned())
            }),
        "response.file_search_call.searching" => tool_started("file_search"),
        "response.file_search_call.completed" => ProviderEvent::ToolDone {
            name: "file_search".to_owned(),
            details: json!({"files_searched": value
                .and_then(|v| v.get("results"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)}),
        },
        "response.web_search_call.searching" => tool_started("web_search"),
        "response.web_search_call.completed" => ProviderEvent::ToolDone {
            name: "web_search".to_owned(),
            details: json!({}),
        },
        "response.code_interpreter_call.in_progress" => tool_started("code_interpreter"),
        "response.output_item.done" => value.map_or(ProviderEvent::Ignored, code_interpreter_done),
        "response.completed" => ProviderEvent::Completed(completion(value, false)),
        "response.incomplete" => ProviderEvent::Completed(completion(value, true)),
        "response.failed" | "error" => ProviderEvent::Failed(failure(value, data, name == "error")),
        _ => ProviderEvent::Ignored,
    }
}

fn tool_started(name: &str) -> ProviderEvent {
    ProviderEvent::ToolStarted {
        name: name.to_owned(),
    }
}

/// `output_item.done` of a `code_interpreter_call`: the joined `logs` outputs,
/// capped at 8192 characters. Every other item is noise.
fn code_interpreter_done(value: &Value) -> ProviderEvent {
    let Some(item) = value.get("item") else {
        return ProviderEvent::Ignored;
    };
    if item.get("type").and_then(Value::as_str) != Some("code_interpreter_call") {
        return ProviderEvent::Ignored;
    }
    let logs = item
        .get("outputs")
        .and_then(Value::as_array)
        .map(|outputs| {
            outputs
                .iter()
                .filter(|output| output.get("type").and_then(Value::as_str) == Some("logs"))
                .filter_map(|output| output.get("logs").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    ProviderEvent::ToolDone {
        name: "code_interpreter".to_owned(),
        details: json!({"output": cap_output(logs)}),
    }
}

fn cap_output(output: String) -> String {
    if output.chars().count() <= CODE_OUTPUT_MAX_CHARS {
        return output;
    }
    let mut capped: String = output.chars().take(CODE_OUTPUT_MAX_CHARS).collect();
    capped.push_str(TRUNCATED_SUFFIX);
    capped
}

fn token_count(usage: Option<&Value>, key: &str) -> u64 {
    usage
        .and_then(|usage| usage.get(key))
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

/// A count nested one level down (`usage.<details>.<key>`); the Chat
/// Completions style `details` alias is accepted next to the Responses one.
fn detail_count(usage: Option<&Value>, details: [&str; 2], key: &str) -> u64 {
    details
        .into_iter()
        .find_map(|name| usage?.get(name)?.get(key)?.as_u64())
        .unwrap_or_default()
}

/// The cache and reasoning breakdown of a provider usage object. The `OpenAI`
/// Responses API has no cache-write count, so `cache_write_input_tokens` is 0.
fn token_breakdown(usage: Option<&Value>) -> TokenBreakdown {
    TokenBreakdown {
        cache_read: detail_count(
            usage,
            ["input_tokens_details", "prompt_tokens_details"],
            "cached_tokens",
        ),
        cache_write: 0,
        reasoning: detail_count(
            usage,
            ["output_tokens_details", "completion_tokens_details"],
            "reasoning_tokens",
        ),
    }
}

fn completion(value: Option<&Value>, incomplete: bool) -> Completion {
    let response = value.and_then(|v| v.get("response"));
    let usage = response.and_then(|r| r.get("usage"));
    Completion {
        response_id: response
            .and_then(|r| r.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        incomplete,
        input_tokens: token_count(usage, "input_tokens"),
        output_tokens: token_count(usage, "output_tokens"),
        breakdown: token_breakdown(usage),
        annotations: response.map(annotations).unwrap_or_default(),
    }
}

/// Annotations of the final response: `output[*].content[*]` parts of type
/// `output_text`, in order. Annotation types other than `url_citation` and
/// `file_citation` are dropped.
fn annotations(response: &Value) -> Vec<Annotation> {
    let mut found = Vec::new();
    let parts = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"));
    for part in parts {
        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
        let list = part
            .get("annotations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        for annotation in list {
            match annotation.get("type").and_then(Value::as_str) {
                Some("url_citation") => found.push(web_annotation(annotation, text)),
                Some("file_citation") => found.push(Annotation::File {
                    file_id: string_field(annotation, "file_id"),
                    filename: string_field(annotation, "filename"),
                }),
                _ => {}
            }
        }
    }
    found
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn web_annotation(annotation: &Value, text: &str) -> Annotation {
    let start = annotation.get("start_index").and_then(Value::as_u64);
    let end = annotation.get("end_index").and_then(Value::as_u64);
    let snippet = annotation.get("text").and_then(Value::as_str).map_or_else(
        || range_snippet(text, start, end),
        std::borrow::ToOwned::to_owned,
    );
    Annotation::Web {
        url: string_field(annotation, "url"),
        title: string_field(annotation, "title"),
        snippet,
        span: start.zip(end),
    }
}

/// The characters `[start, end)` of `text` (character offsets, not bytes); a
/// range outside the text gives an empty snippet.
fn range_snippet(text: &str, start: Option<u64>, end: Option<u64>) -> String {
    let (Some(start), Some(end)) = (start, end) else {
        return String::new();
    };
    let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
        return String::new();
    };
    if end <= start {
        return String::new();
    }
    text.chars().skip(start).take(end - start).collect()
}

/// `response.failed` / `error`: the error of the response, else the top-level
/// `error` object, else (for the `error` event) the flat `{code, message}`,
/// else the unparseable data itself.
fn failure(value: Option<&Value>, raw: &str, error_event: bool) -> Failure {
    let Some(value) = value else {
        return Failure {
            message: raw.to_owned(),
            ..Failure::default()
        };
    };
    let usage = value.get("response").and_then(|r| r.get("usage"));
    let error = [
        value.get("response").and_then(|r| r.get("error")),
        value.get("error"),
    ]
    .into_iter()
    .flatten()
    .find(|error| !error.is_null());
    let (code, message) = match error {
        Some(Value::String(message)) => (None, message.clone()),
        Some(error) => (
            error.get("code").and_then(Value::as_str).map(str::to_owned),
            string_field(error, "message"),
        ),
        None if error_event => (
            value.get("code").and_then(Value::as_str).map(str::to_owned),
            string_field(value, "message"),
        ),
        None => (None, String::new()),
    };
    Failure {
        code,
        message,
        input_tokens: token_count(usage, "input_tokens"),
        output_tokens: token_count(usage, "output_tokens"),
        breakdown: token_breakdown(usage),
    }
}

// ── non-streaming response ───────────────────────────────────────────────────

/// Translates the JSON body of a non-streaming Responses API call (the summary
/// call) into the events its streaming twin would have produced: the text of
/// every `output[*].content[*]` part of type `output_text` as one
/// `TextDelta`, then `Completed` (`status` `completed` / `incomplete`, with the
/// usage) or `Failed` (`status` `failed` or an `error` object).
///
/// # Errors
/// A [`ProviderFailure`] when the body is not a JSON object.
pub fn parse_response_body(body: &[u8]) -> Result<Vec<ProviderEvent>, ProviderFailure> {
    let Ok(response @ Value::Object(_)) = serde_json::from_slice::<Value>(body) else {
        return Err(ProviderFailure::error(
            "the provider answered with an unexpected response",
        ));
    };
    // The streaming events carry the response under `response`.
    let wrapped = json!({"response": response});
    let has_error = response.get("error").is_some_and(|error| !error.is_null());
    let status = response.get("status").and_then(Value::as_str);
    if has_error || status == Some("failed") {
        return Ok(vec![ProviderEvent::Failed(failure(
            Some(&wrapped),
            "",
            false,
        ))]);
    }
    let text: String = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    let mut events = Vec::with_capacity(2);
    if !text.is_empty() {
        events.push(ProviderEvent::TextDelta(text));
    }
    events.push(ProviderEvent::Completed(completion(
        Some(&wrapped),
        status == Some("incomplete"),
    )));
    Ok(events)
}

/// Whether the response is a server-sent event stream.
fn is_event_stream(response: &http::Response<Body>) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
}

/// The whole body of a non-streaming answer, bounded by
/// [`RESPONSE_BODY_LIMIT`].
async fn read_body(body: Body) -> Result<Vec<u8>, ProviderFailure> {
    let mut stream = body.into_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk: Bytes = chunk.map_err(|error| {
            tracing::warn!(%error, "provider response body failed");
            ProviderFailure::error("the provider response was interrupted")
        })?;
        buffer.extend_from_slice(&chunk);
        if buffer.len() > RESPONSE_BODY_LIMIT {
            return Err(ProviderFailure::error("the provider response is too large"));
        }
    }
    Ok(buffer)
}

// ── request building ─────────────────────────────────────────────────────────

/// `{tenant_id_hex}{user_id_hex}`: both ids in simple form, exactly 64
/// characters (the provider limit of the `user` field).
fn user_field(request: &ChatRequest) -> String {
    format!("{}{}", request.tenant_id.simple(), request.user_id.simple())
}

/// `none`, or the tools of the request joined with `+`.
fn feature(tools: &[ChatTool]) -> String {
    if tools.is_empty() {
        return "none".to_owned();
    }
    tools
        .iter()
        .map(ChatTool::feature)
        .collect::<Vec<_>>()
        .join("+")
}

fn tool_json(tool: &ChatTool) -> Value {
    match tool {
        ChatTool::FileSearch {
            vector_store_ids,
            max_num_results,
        } => json!({
            "type": "file_search",
            "vector_store_ids": vector_store_ids,
            "max_num_results": max_num_results,
        }),
        ChatTool::WebSearch { context_size } => json!({
            "type": "web_search",
            "search_context_size": context_size,
        }),
        ChatTool::CodeInterpreter { file_ids } => json!({
            "type": "code_interpreter",
            "container": {"type": "auto", "file_ids": file_ids},
        }),
    }
}

/// The JSON body of a Responses API call.
#[must_use]
pub fn build_body(request: &ChatRequest) -> Value {
    let input: Vec<Value> = request
        .input
        .iter()
        .map(|item| {
            let (role, kind) = match item.role {
                HistoryRole::User => ("user", "input_text"),
                HistoryRole::Assistant => ("assistant", "output_text"),
            };
            let mut content = vec![json!({"type": kind, "text": item.content})];
            content.extend(
                item.images
                    .iter()
                    .map(|file_id| json!({"type": "input_image", "file_id": file_id})),
            );
            json!({"role": role, "content": content})
        })
        .collect();

    let mut body = Map::new();
    body.insert("model".to_owned(), json!(request.provider_model_id));
    // Chat turns stream; the summary call (a system task nobody watches) is a
    // plain request / response.
    body.insert("stream".to_owned(), json!(!request.is_summary));
    if !request.instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(request.instructions));
    }
    body.insert("input".to_owned(), Value::Array(input));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request.tools.iter().map(tool_json).collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    // The code interpreter's output is only returned when asked for.
    if request
        .tools
        .iter()
        .any(|tool| matches!(tool, ChatTool::CodeInterpreter { .. }))
    {
        body.insert(
            "include".to_owned(),
            json!(["code_interpreter_call.outputs"]),
        );
    }
    body.insert(
        "max_output_tokens".to_owned(),
        json!(request.max_output_tokens),
    );
    if !request.is_summary {
        body.insert("max_tool_calls".to_owned(), json!(request.max_tool_calls));
    }
    body.insert("user".to_owned(), json!(user_field(request)));
    body.insert(
        "metadata".to_owned(),
        json!({
            "tenant_id": request.tenant_id.to_string(),
            "user_id": request.user_id.to_string(),
            "chat_id": request.chat_id.to_string(),
            "request_type": if request.is_summary { "summary" } else { "chat" },
            "feature": feature(&request.tools),
        }),
    );

    let params = &request.api_params;
    for (key, value) in [
        ("temperature", params.temperature),
        ("top_p", params.top_p),
        ("frequency_penalty", params.frequency_penalty),
        ("presence_penalty", params.presence_penalty),
    ] {
        if let Some(value) = value {
            body.insert(key.to_owned(), json!(value));
        }
    }
    if let Some(effort) = &params.reasoning_effort {
        body.insert("reasoning".to_owned(), json!({"effort": effort}));
    }
    if let Some(Value::Object(extra)) = &params.extra_body {
        for (key, value) in extra {
            if CONTROLLED_KEYS.contains(&key.as_str()) {
                tracing::warn!(
                    key,
                    "api_params.extra_body key is controlled by the gear; ignored"
                );
            } else {
                body.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(body)
}

/// Percent-encodes everything but the RFC 3986 unreserved characters.
pub(super) fn encode_path_segment(raw: &str) -> String {
    let mut encoded = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.extend(format!("%{byte:02X}").chars());
        }
    }
    encoded
}

/// The proxied request: `POST /{alias}{api_path}` with the JSON body. No
/// credential header is set: OAGW injects the provider credentials.
fn build_request(
    alias: &str,
    api_path: &str,
    request: &ChatRequest,
) -> Result<http::Request<Body>, ProviderFailure> {
    let path = api_path.replace("{model}", &encode_path_segment(&request.provider_model_id));
    let separator = if path.starts_with('/') { "" } else { "/" };
    let body = serde_json::to_vec(&build_body(request)).map_err(|error| {
        tracing::error!(%error, "provider request body does not serialize");
        ProviderFailure::error("the provider request could not be built")
    })?;
    http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("/{alias}{separator}{path}"))
        .header(CONTENT_TYPE, "application/json")
        .header(
            ACCEPT,
            if request.is_summary {
                "application/json"
            } else {
                "text/event-stream"
            },
        )
        .body(Body::from(body))
        .map_err(|error| {
            tracing::error!(%error, "provider request does not build");
            ProviderFailure::error("the provider request could not be built")
        })
}

// ── calling the provider ─────────────────────────────────────────────────────

/// Starts a Responses API call through OAGW and returns the translated event
/// stream. A chat call streams; the summary call is non-streaming, and its JSON
/// body is translated into the same events (a `text/event-stream` answer is
/// accepted as well: the response `Content-Type` decides). Dropping the stream
/// drops the provider response body (the hard cancel). The gear never retries.
///
/// # Errors
/// A [`ProviderFailure`]: `rate_limited` for a 429, `provider_timeout` for the
/// gateway's own deadline, `provider_error` for everything else.
pub async fn stream_chat(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    alias: &str,
    api_path: &str,
    request: &ChatRequest,
) -> Result<ProviderEventStream, ProviderFailure> {
    let http_request = build_request(alias, api_path, request)?;
    let response = oagw
        .proxy_request(ctx.clone(), http_request)
        .await
        .map_err(|error| gateway_failure(&error))?;
    if !response.status().is_success() {
        return Err(http_failure(response).await);
    }
    if request.is_summary && !is_event_stream(&response) {
        let body = read_body(response.into_body()).await?;
        let events = parse_response_body(&body)?;
        return Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))));
    }
    match ServerEventsStream::from_response::<ServerEvent>(response) {
        ServerEventsResponse::Events(events) => Ok(Box::pin(events.map(|event| match event {
            Ok(event) => Ok(parse_event(
                event.event.as_deref().unwrap_or_default(),
                &event.data,
            )),
            Err(error) => {
                tracing::warn!(%error, "provider event stream failed");
                Err(ProviderFailure::error(
                    "the provider stream was interrupted",
                ))
            }
        }))),
        ServerEventsResponse::Response(_) => Err(ProviderFailure::error(
            "the provider answered with an unexpected response",
        )),
    }
}

fn failure_of(kind: ProviderErrorKind, message: &str) -> ProviderFailure {
    ProviderFailure {
        kind,
        message: message.to_owned(),
        context_length_exceeded: false,
    }
}

/// A failure of the gateway itself: only its own deadline is a timeout.
fn gateway_failure(error: &CanonicalError) -> ProviderFailure {
    tracing::warn!(
        category = error.gts_type(),
        detail = error.detail(),
        "OAGW proxy call failed"
    );
    match error {
        CanonicalError::DeadlineExceeded { .. } => failure_of(
            ProviderErrorKind::Timeout,
            "The provider did not answer in time",
        ),
        CanonicalError::ResourceExhausted { .. } => failure_of(
            ProviderErrorKind::RateLimited,
            "The provider rate limit was reached",
        ),
        _ => failure_of(
            ProviderErrorKind::Error,
            "The provider is currently unavailable",
        ),
    }
}

/// Text shown for a failure the gateway itself produced (OAGW RFC 9457
/// problem): the problem document names the upstream alias / host, the proxied
/// path and internal type URIs, so it never reaches a client.
const GATEWAY_UNAVAILABLE_MESSAGE: &str = "The provider is currently unavailable";

/// A non-2xx answer: 429 is `rate_limited` (with the numeric `Retry-After`),
/// the gateway's own 504 is a timeout, any other gateway-produced answer is a
/// fixed generic `provider_error` (its body is only logged), and a provider's
/// own answer - including its own 504 - is `provider_error` carrying the
/// sanitized `error.message` / `message` of the provider, or just
/// "The provider returned HTTP <status>" when the body has none.
async fn http_failure(response: http::Response<Body>) -> ProviderFailure {
    let status = response.status();
    let is_problem = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("problem+json"));
    let from_gateway =
        response.extensions().get::<ErrorSource>() == Some(&ErrorSource::Gateway) || is_problem;
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let body = read_limited(response.into_body()).await;

    if status == http::StatusCode::TOO_MANY_REQUESTS {
        let message = match retry_after {
            Some(seconds) => {
                format!("The provider rate limit was reached; retry after {seconds} seconds")
            }
            None => "The provider rate limit was reached".to_owned(),
        };
        return failure_of(ProviderErrorKind::RateLimited, &message);
    }
    if status == http::StatusCode::GATEWAY_TIMEOUT && from_gateway {
        return failure_of(
            ProviderErrorKind::Timeout,
            "The provider did not answer in time",
        );
    }
    if from_gateway {
        tracing::warn!(
            status = status.as_u16(),
            body = %String::from_utf8_lossy(&body),
            "OAGW answered the provider call with its own error"
        );
        return failure_of(ProviderErrorKind::Error, GATEWAY_UNAVAILABLE_MESSAGE);
    }
    let message = match error_text(&body) {
        Some(text) if !text.trim().is_empty() => client_message(&text),
        _ => format!("The provider returned HTTP {}", status.as_u16()),
    };
    ProviderFailure {
        context_length_exceeded: status == http::StatusCode::BAD_REQUEST
            && is_context_length_error(&body),
        ..failure_of(ProviderErrorKind::Error, &message)
    }
}

/// The provider body names the prompt-too-large condition: `error.code` (or a
/// top-level `code`) is `context_length_exceeded`.
fn is_context_length_error(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    [
        value.get("error").and_then(|error| error.get("code")),
        value.get("code"),
    ]
    .into_iter()
    .flatten()
    .any(|code| code.as_str() == Some("context_length_exceeded"))
}

async fn read_limited(body: Body) -> Vec<u8> {
    let mut stream = body.into_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk): Result<Bytes, _> = chunk else {
            break;
        };
        buffer.extend_from_slice(&chunk);
        if buffer.len() >= ERROR_BODY_LIMIT {
            break;
        }
    }
    buffer
}

/// The provider's own message: `error.message`, else `message`, else a
/// string `error`. `None` when the body is not JSON or carries none of them -
/// a raw body is never forwarded.
fn error_text(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    [
        value.get("error").and_then(|error| error.get("message")),
        value.get("message"),
        value.get("error"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .map(str::to_owned)
}
