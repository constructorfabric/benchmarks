//! Anthropic Messages API adapter.
//!
//! Drops `file_search`; maps `web_search` and `code_interpreter` to
//! Anthropic server tools; sends the identity as `metadata.user_id`; returns
//! no citations.

use std::sync::Arc;

use futures::StreamExt;
use oagw_sdk::ServiceGatewayClientV1;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use serde_json::{Map, Value, json};
use toolkit_security::SecurityContext;

use super::{map_error_response, map_gateway_error, normalize_incomplete_reason};
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::{LlmProviderError, LlmRequest, ProviderEvent, ProviderEventStream, ToolSpec};

/// Build the Anthropic Messages request body.
#[must_use]
pub fn build_request_body(request: &LlmRequest) -> Vec<u8> {
    let messages: Vec<Value> = request
        .input
        .iter()
        .map(|msg| {
            let mut content = vec![json!({"type": "text", "text": msg.text})];
            for image in &msg.images {
                // Anthropic-native chats pass the secondary Anthropic file
                // id; other file ids are dropped by the caller.
                content.push(json!({
                    "type": "image",
                    "source": {"type": "file", "file_id": image},
                }));
            }
            json!({"role": msg.role, "content": content})
        })
        .chain(request.trailing_items.iter().map(|item| {
            // Convert Responses-style agentic items to Anthropic content
            // blocks.
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => json!({
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                        "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "input": serde_json::from_str::<Value>(
                            item.get("arguments").and_then(Value::as_str).unwrap_or("{}"),
                        )
                        .unwrap_or(Value::Null),
                    }],
                }),
                Some("function_call_output") => json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                        "content": item.get("output").and_then(Value::as_str).unwrap_or_default(),
                    }],
                }),
                _ => Value::Null,
            }
        }))
        .filter(|m| !m.is_null())
        .collect();

    let tools: Vec<Value> = request
        .tools
        .iter()
        .filter_map(|tool| match tool {
            ToolSpec::WebSearch { .. } => Some(json!({
                "type": "web_search_20250305",
                "name": "web_search",
            })),
            ToolSpec::CodeInterpreter { .. } => Some(json!({
                "type": "code_execution_20250825",
                "name": "code_execution",
            })),
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => Some(json!({
                "name": name,
                "description": description,
                "input_schema": parameters,
            })),
            ToolSpec::FileSearch { .. } => None,
        })
        .collect();

    let mut body = Map::new();
    body.insert("model".to_owned(), Value::from(request.provider_model_id.clone()));
    body.insert("messages".to_owned(), Value::Array(messages));
    if !request.instructions.is_empty() {
        body.insert(
            "system".to_owned(),
            Value::from(request.instructions.clone()),
        );
    }
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    body.insert(
        "max_tokens".to_owned(),
        Value::from(request.max_output_tokens),
    );
    body.insert("stream".to_owned(), Value::from(request.stream));
    body.insert(
        "metadata".to_owned(),
        json!({"user_id": request.user}),
    );
    let params = &request.api_params;
    if let Some(temperature) = params.temperature {
        body.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = params.top_p {
        body.insert("top_p".to_owned(), json!(top_p));
    }
    if !params.stop.is_empty() {
        body.insert("stop_sequences".to_owned(), json!(params.stop));
    }
    serde_json::to_vec(&Value::Object(body)).unwrap_or_default()
}

fn anthropic_request(
    provider: &ResolvedProvider,
    path: &str,
    body: Vec<u8>,
) -> Result<http::Request<oagw_sdk::Body>, LlmProviderError> {
    http::Request::builder()
        .method(http::Method::POST)
        .uri(format!("/{}{path}", provider.alias))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(oagw_sdk::Body::from(body))
        .map_err(|e| LlmProviderError::InvalidResponse(format!("request build failed: {e}")))
}

/// Send a streaming chat request.
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
    let req = anthropic_request(provider, &path, body)?;
    let resp = gateway
        .proxy_request(ctx.clone(), req)
        .await
        .map_err(map_gateway_error)?;
    match ServerEventsStream::from_response::<ServerEvent>(resp) {
        ServerEventsResponse::Events(stream) => Ok(Box::pin(event_stream(stream))),
        ServerEventsResponse::Response(resp) => Err(map_error_response(resp).await),
    }
}

/// Non-streaming completion (thread summary).
pub async fn complete(
    gateway: &Arc<dyn ServiceGatewayClientV1>,
    ctx: &SecurityContext,
    provider: &ResolvedProvider,
    request: &LlmRequest,
) -> Result<(String, Option<mini_chat_sdk::UsageTokens>, Option<String>), LlmProviderError> {
    let mut request = request.clone();
    request.stream = false;
    let body = build_request_body(&request);
    let path = provider
        .api_path
        .replace("{model}", &request.provider_model_id);
    let req = anthropic_request(provider, &path, body)?;
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
    let mut text = String::new();
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(t) = block.get("text").and_then(Value::as_str)
            {
                text.push_str(t);
            }
        }
    }
    let usage = value.get("usage").map(|u| mini_chat_sdk::UsageTokens {
        input_tokens: u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0),
        output_tokens: u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0),
        cache_read_input_tokens: u
            .get("cache_read_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        cache_write_input_tokens: u
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        reasoning_tokens: 0,
    });
    let id = value.get("id").and_then(Value::as_str).map(str::to_owned);
    Ok((text, usage, id))
}

/// Streaming state.
#[derive(Default)]
struct StreamState {
    /// Input tokens from `message_start`.
    input_tokens: i64,
    /// Cache tokens from `message_start`.
    cache_read: i64,
    cache_write: i64,
    /// Output tokens from `message_delta`.
    output_tokens: i64,
    /// Message id.
    id: Option<String>,
    /// Current tool block: (name, `call_id`, accumulated JSON args).
    tool: Option<(String, String, String)>,
    /// Queued events.
    pending: std::collections::VecDeque<ProviderEvent>,
}

fn event_stream(
    stream: ServerEventsStream<ServerEvent>,
) -> impl futures::Stream<Item = Result<ProviderEvent, LlmProviderError>> + Send {
    let state = StreamState::default();
    futures::stream::unfold(
        (Box::pin(stream), state),
        |(mut stream, mut state)| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), (stream, state)));
                }
                let event = match stream.next().await {
                    Some(Ok(event)) => event,
                    Some(Err(err)) => {
                        return Some((
                            Err(LlmProviderError::Unavailable(format!("stream error: {err}"))),
                            (stream, state),
                        ));
                    }
                    None => return None,
                };
                let data = event.data.trim();
                if data.is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let name = value.get("type").and_then(Value::as_str).unwrap_or("");
                match name {
                    "message_start" => {
                        let message = value.get("message").cloned().unwrap_or(Value::Null);
                        state.id = message.get("id").and_then(Value::as_str).map(str::to_owned);
                        if let Some(usage) = message.get("usage") {
                            state.input_tokens =
                                usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0);
                            state.cache_read = usage
                                .get("cache_read_input_tokens")
                                .and_then(Value::as_i64)
                                .unwrap_or(0);
                            state.cache_write = usage
                                .get("cache_creation_input_tokens")
                                .and_then(Value::as_i64)
                                .unwrap_or(0);
                        }
                    }
                    "content_block_start" => {
                        let block = value.get("content_block").cloned().unwrap_or(Value::Null);
                        match block.get("type").and_then(Value::as_str) {
                            Some("server_tool_use" | "tool_use") => {
                                let tool_name = block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                let call_id = block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                                let mapped = match tool_name {
                                    "web_search" => "web_search",
                                    "code_execution" => "code_interpreter",
                                    other => other,
                                };
                                state.tool = Some((tool_name.to_owned(), call_id, String::new()));
                                state.pending.push_back(ProviderEvent::ToolStart {
                                    name: mapped.to_owned(),
                                    details: json!({}),
                                });
                            }
                            _ => {}
                        }
                    }
                    "content_block_delta" => {
                        let delta = value.get("delta").cloned().unwrap_or(Value::Null);
                        match delta.get("type").and_then(Value::as_str) {
                            Some("text_delta") => {
                                if let Some(text) =
                                    delta.get("text").and_then(Value::as_str)
                                    && !text.is_empty()
                                {
                                    state
                                        .pending
                                        .push_back(ProviderEvent::TextDelta(text.to_owned()));
                                }
                            }
                            Some("input_json_delta") => {
                                if let (Some(tool), Some(partial)) = (
                                    state.tool.as_mut(),
                                    delta.get("partial_json").and_then(Value::as_str),
                                ) {
                                    tool.2.push_str(partial);
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        if let Some((name, call_id, args)) = state.tool.take() {
                            match name.as_str() {
                                "web_search" | "code_execution" => {
                                    let mapped = if name == "web_search" {
                                        "web_search"
                                    } else {
                                        "code_interpreter"
                                    };
                                    state.pending.push_back(ProviderEvent::ToolDone {
                                        name: mapped.to_owned(),
                                        details: json!({}),
                                    });
                                }
                                _ => {
                                    state.pending.push_back(ProviderEvent::FunctionCall {
                                        name,
                                        call_id,
                                        arguments: args,
                                    });
                                }
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = value.get("usage") {
                            state.output_tokens =
                                usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(0);
                        }
                        let stop_reason = value
                            .get("delta")
                            .and_then(|d| d.get("stop_reason"))
                            .and_then(Value::as_str);
                        let incomplete = normalize_incomplete_reason(stop_reason);
                        state.pending.push_back(ProviderEvent::Completed {
                            usage: Some(mini_chat_sdk::UsageTokens {
                                input_tokens: state.input_tokens,
                                output_tokens: state.output_tokens,
                                cache_read_input_tokens: state.cache_read,
                                cache_write_input_tokens: state.cache_write,
                                reasoning_tokens: 0,
                            }),
                            provider_response_id: state.id.clone(),
                            incomplete_reason: incomplete,
                        });
                    }
                    "message_stop" => {}
                    "error" => {
                        let message = value
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("provider error");
                        state.pending.push_back(ProviderEvent::Failed {
                            code: "provider_error".to_owned(),
                            message: crate::infra::sanitize::sanitize_provider_message(message),
                            usage: None,
                            provider_response_id: state.id.clone(),
                        });
                    }
                    _ => {}
                }
            }
        },
    )
}
