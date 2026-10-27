//! `OpenAI` / Azure `OpenAI` `Responses` API adapter.

use std::sync::Arc;

use futures::StreamExt;
use mini_chat_sdk::UsageTokens;
use oagw_sdk::ServiceGatewayClientV1;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use serde_json::{Map, Value, json};
use toolkit_security::SecurityContext;

use super::{json_request, map_error_response, map_gateway_error, normalize_incomplete_reason, parse_usage};
use crate::api::rest::dto::CitationSource;
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::{
    LlmProviderError, LlmRequest, ProviderEvent, ProviderEventStream, RawCitation, ToolSpec,
};
use crate::infra::sanitize::sanitize_provider_message;

/// Keys the request controls; `extra_body` entries with these are ignored
/// with a warning.
pub(crate) const CONTROLLED_KEYS: &[&str] = &[
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

/// Build the `Responses` API request body.
#[must_use]
pub fn build_request_body(request: &LlmRequest) -> Vec<u8> {
    let input: Vec<Value> = request
        .input
        .iter()
        .map(|msg| {
            let mut content = Vec::new();
            match msg.role.as_str() {
                "assistant" => {
                    content.push(json!({"type": "output_text", "text": msg.text}));
                }
                _ => {
                    content.push(json!({"type": "input_text", "text": msg.text}));
                    for image in &msg.images {
                        content.push(json!({"type": "input_image", "file_id": image}));
                    }
                }
            }
            json!({"role": msg.role, "content": content})
        })
        .chain(request.trailing_items.iter().cloned())
        .collect();

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| match tool {
            ToolSpec::FileSearch {
                vector_store_ids,
                max_num_results,
            } => json!({
                "type": "file_search",
                "vector_store_ids": vector_store_ids,
                "max_num_results": max_num_results,
            }),
            ToolSpec::WebSearch {
                search_context_size,
            } => {
                let size = match search_context_size {
                    mini_chat_sdk::WebSearchContextSize::Low => "low",
                    mini_chat_sdk::WebSearchContextSize::Medium => "medium",
                    mini_chat_sdk::WebSearchContextSize::High => "high",
                };
                json!({"type": "web_search", "search_context_size": size})
            }
            ToolSpec::CodeInterpreter { file_ids } => json!({
                "type": "code_interpreter",
                "container": {"type": "auto", "file_ids": file_ids},
            }),
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": parameters,
            }),
        })
        .collect();

    let mut body = Map::new();
    body.insert("model".to_owned(), Value::from(request.provider_model_id.clone()));
    body.insert("input".to_owned(), Value::Array(input));
    body.insert(
        "instructions".to_owned(),
        Value::from(request.instructions.clone()),
    );
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    body.insert(
        "max_output_tokens".to_owned(),
        Value::from(request.max_output_tokens),
    );
    body.insert(
        "max_tool_calls".to_owned(),
        Value::from(request.max_tool_calls),
    );
    body.insert("user".to_owned(), Value::from(request.user.clone()));
    if let Some(metadata) = &request.metadata {
        body.insert("metadata".to_owned(), Value::Object(metadata.clone()));
    }
    body.insert("stream".to_owned(), Value::from(request.stream));
    if request
        .tools
        .iter()
        .any(|t| matches!(t, ToolSpec::CodeInterpreter { .. }))
    {
        body.insert(
            "include".to_owned(),
            json!(["code_interpreter_call.outputs"]),
        );
    }
    let params = &request.api_params;
    if let Some(temperature) = params.temperature {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = params.top_p {
        body.insert("top_p".to_owned(), json!(top_p));
    }
    if let Some(frequency_penalty) = params.frequency_penalty {
        body.insert("frequency_penalty".to_owned(), json!(frequency_penalty));
    }
    if let Some(presence_penalty) = params.presence_penalty {
        body.insert("presence_penalty".to_owned(), json!(presence_penalty));
    }
    if !params.stop.is_empty() {
        body.insert("stop".to_owned(), json!(params.stop));
    }
    if let Some(effort) = &params.reasoning_effort {
        body.insert("reasoning".to_owned(), json!({"effort": effort}));
    }
    if let Some(extra) = &params.extra_body {
        for (key, value) in extra {
            if CONTROLLED_KEYS.contains(&key.as_str()) {
                tracing::warn!("mini-chat: extra_body key '{key}' is controlled by the request; ignored");
                continue;
            }
            body.insert(key.clone(), value.clone());
        }
    }
    serde_json::to_vec(&Value::Object(body)).unwrap_or_default()
}

/// Send a streaming chat request and parse the provider SSE stream into
/// internal events.
pub async fn stream_chat(
    gateway: &Arc<dyn ServiceGatewayClientV1>,
    ctx: &SecurityContext,
    provider: &ResolvedProvider,
    request: &LlmRequest,
) -> Result<ProviderEventStream, LlmProviderError> {
    let body = build_request_body(request);
    let path = provider
        .api_path
        .replace("{model}", &request.provider_model_id);
    let req = json_request(provider, &path, body)?;
    let resp = gateway
        .proxy_request(ctx.clone(), req)
        .await
        .map_err(map_gateway_error)?;
    match ServerEventsStream::from_response::<ServerEvent>(resp) {
        ServerEventsResponse::Events(stream) => Ok(Box::pin(event_stream(stream))),
        ServerEventsResponse::Response(resp) => Err(map_error_response(resp).await),
    }
}

/// Send a non-streaming completion (thread summary).
pub async fn complete(
    gateway: &Arc<dyn ServiceGatewayClientV1>,
    ctx: &SecurityContext,
    provider: &ResolvedProvider,
    request: &LlmRequest,
) -> Result<(String, Option<UsageTokens>, Option<String>), LlmProviderError> {
    let mut request = request.clone();
    request.stream = false;
    let body = build_request_body(&request);
    let path = provider
        .api_path
        .replace("{model}", &request.provider_model_id);
    let req = json_request(provider, &path, body)?;
    let resp = gateway
        .proxy_request(ctx.clone(), req)
        .await
        .map_err(map_gateway_error)?;
    if !resp.status().is_success() {
        return Err(map_error_response(resp).await);
    }
    let bytes = resp
        .into_body()
        .into_bytes()
        .await
        .map_err(|e| LlmProviderError::Unavailable(format!("failed to read response: {e}")))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|e| LlmProviderError::InvalidResponse(format!("invalid JSON: {e}")))?;
    let text = extract_output_text(&value);
    let usage = parse_usage(&value);
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((text, usage, id))
}

/// Extract the concatenated output text of a `Responses` API response.
#[must_use]
pub fn extract_output_text(response: &Value) -> String {
    let mut text = String::new();
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(t) = part.get("text").and_then(Value::as_str)
                    {
                        text.push_str(t);
                    }
                }
            }
        }
    }
    text
}

/// Extract citations from the annotations of a completed response.
#[must_use]
pub fn extract_citations(response: &Value) -> Vec<RawCitation> {
    let mut citations = Vec::new();
    let Some(output) = response.get("output").and_then(Value::as_array) else {
        return citations;
    };
    for item in output {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(Value::as_str) != Some("output_text") {
                continue;
            }
            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
            let Some(annotations) = part.get("annotations").and_then(Value::as_array) else {
                continue;
            };
            for annotation in annotations {
                match annotation.get("type").and_then(Value::as_str) {
                    Some("url_citation") => {
                        let span = annotation
                            .get("start_index")
                            .and_then(Value::as_u64)
                            .zip(annotation.get("end_index").and_then(Value::as_u64))
                            .map(|(s, e)| {
                                (u32::try_from(s).unwrap_or(u32::MAX), u32::try_from(e).unwrap_or(u32::MAX))
                            });
                        let snippet = span.map(|(start, end)| {
                            let chars: Vec<char> = text.chars().collect();
                            let (s, e) = (start as usize, end as usize);
                            if s <= e && e <= chars.len() {
                                chars[s..e].iter().collect()
                            } else {
                                String::new()
                            }
                        });
                        citations.push(RawCitation {
                            source: CitationSource::Web,
                            provider_file_id: None,
                            filename: None,
                            url: annotation
                                .get("url")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            title: annotation
                                .get("title")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            span,
                            snippet,
                        });
                    }
                    Some("file_citation") => {
                        citations.push(RawCitation {
                            source: CitationSource::File,
                            provider_file_id: annotation
                                .get("file_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            filename: annotation
                                .get("filename")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            url: None,
                            title: None,
                            span: None,
                            snippet: Some(String::new()),
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    citations
}

#[allow(clippy::redundant_continue, reason = "the event loop is clearest with explicit continue")]
/// The parsed event stream over a `Responses` SSE stream.
pub(crate) fn event_stream(
    stream: ServerEventsStream<ServerEvent>,
) -> impl futures::Stream<Item = Result<ProviderEvent, LlmProviderError>> {
    futures::stream::unfold(Box::pin(stream), |mut stream| async move {
        loop {
            let event = match stream.next().await {
                Some(Ok(event)) => event,
                Some(Err(err)) => {
                    return Some((
                        Err(LlmProviderError::Unavailable(format!("stream error: {err}"))),
                        stream,
                    ));
                }
                None => return None,
            };
            if event.is_empty() {
                continue;
            }

            match map_server_event(&event) {
                EventMapping::Event(parsed) => return Some((parsed, stream)),
                EventMapping::Skip => continue,
            }
        }
    })
}

enum EventMapping {
    Event(Result<ProviderEvent, LlmProviderError>),
    Skip,
}

/// Map one provider SSE event to an internal event.
fn map_server_event(event: &ServerEvent) -> EventMapping {
    let Some(data) = event_data(event) else {
        return EventMapping::Skip;
    };
    // Event name from the SSE `event:` line; fall back to the data `type`.
    let name = match event.event.as_deref() {
        Some("message") | None => data.get("type").and_then(Value::as_str).unwrap_or(""),
        Some(name) => name,
    };
    match map_named_event(name, &data) {
        Some(mapped) => EventMapping::Event(mapped),
        None => EventMapping::Skip,
    }
}

fn event_data(event: &ServerEvent) -> Option<Value> {
    let data = event.data.trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    match serde_json::from_str(data) {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::debug!("mini-chat: unparseable provider SSE data: {err}");
            None
        }
    }
}

#[allow(clippy::redundant_continue, reason = "event-mapping loop is clearest with explicit continue")]
/// Map a named `Responses` event; `None` skips the event.
fn map_named_event(name: &str, data: &Value) -> Option<Result<ProviderEvent, LlmProviderError>> {
    match name {
        "response.output_text.delta" => {
            let delta = data
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some(Ok(ProviderEvent::TextDelta(delta)))
        }
        "response.file_search_call.searching" => Some(Ok(ProviderEvent::ToolStart {
            name: "file_search".to_owned(),
            details: json!({}),
        })),
        "response.file_search_call.completed" => {
            let files_searched = data
                .get("results")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Some(Ok(ProviderEvent::ToolDone {
                name: "file_search".to_owned(),
                details: json!({"files_searched": files_searched}),
            }))
        }
        "response.web_search_call.searching" => Some(Ok(ProviderEvent::ToolStart {
            name: "web_search".to_owned(),
            details: json!({}),
        })),
        "response.web_search_call.completed" => Some(Ok(ProviderEvent::ToolDone {
            name: "web_search".to_owned(),
            details: json!({}),
        })),
        "response.code_interpreter_call.in_progress" => Some(Ok(ProviderEvent::ToolStart {
            name: "code_interpreter".to_owned(),
            details: json!({}),
        })),
        "response.output_item.done" => {
            let item = data.get("item").cloned().unwrap_or(Value::Null);
            match item.get("type").and_then(Value::as_str) {
                Some("code_interpreter_call") => {
                    let logs = item
                        .pointer("/logs")
                        .or_else(|| item.pointer("/outputs"))
                        .and_then(Value::as_array)
                        .map(|outputs| {
                            outputs
                                .iter()
                                .filter_map(|o| o.get("logs").and_then(Value::as_str).map(str::to_owned))
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    let output = if logs.chars().count() > 8192 {
                        let truncated: String = logs.chars().take(8192).collect();
                        format!("{truncated}...[truncated]")
                    } else {
                        logs
                    };
                    Some(Ok(ProviderEvent::ToolDone {
                        name: "code_interpreter".to_owned(),
                        details: json!({"output": output}),
                    }))
                }
                Some("function_call") => Some(Ok(ProviderEvent::FunctionCall {
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    call_id: item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                })),
                _ => None,
            }
        }
        "response.completed" | "response.incomplete" => {
            let response = data.get("response").cloned().unwrap_or(Value::Null);
            let usage = parse_usage(&response);
            let id = response.get("id").and_then(Value::as_str).map(str::to_owned);
            let citations = extract_citations(&response);
            let incomplete_reason = if name == "response.incomplete" {
                normalize_incomplete_reason(
                    response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str),
                )
            } else {
                None
            };
            Some(Ok(ProviderEvent::Completed {
                usage,
                provider_response_id: id,
                incomplete_reason,
            }
            .with_citations(citations)))
        }
        "response.failed" => {
            let response = data.get("response").cloned().unwrap_or(Value::Null);
            let message = response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| data.pointer("/error/message").and_then(Value::as_str))
                .unwrap_or("provider error");
            Some(Ok(ProviderEvent::Failed {
                code: "provider_error".to_owned(),
                message: sanitize_provider_message(message),
                usage: parse_usage(&response),
                provider_response_id: response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }))
        }
        "error" => {
            let message = data
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| data.get("message").and_then(Value::as_str))
                .unwrap_or("provider error");
            Some(Ok(ProviderEvent::Failed {
                code: "provider_error".to_owned(),
                message: sanitize_provider_message(message),
                usage: None,
                provider_response_id: None,
            }))
        }
        _ => None,
    }
}

impl ProviderEvent {
    /// Attach citations to a `Completed` event (kept structurally separate
    /// in the enum; the stream service unwraps them).
    fn with_citations(self, citations: Vec<RawCitation>) -> Self {
        if citations.is_empty() {
            return self;
        }
        match self {
            ProviderEvent::Completed {
                usage,
                provider_response_id,
                incomplete_reason,
            } => ProviderEvent::CompletedWithCitations {
                usage,
                provider_response_id,
                incomplete_reason,
                citations,
            },
            other => other,
        }
    }
}
