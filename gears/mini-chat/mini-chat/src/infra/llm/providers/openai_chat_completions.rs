//! `OpenAI` Chat Completions adapter.
//!
//! Drops `file_search`, `web_search` and `code_interpreter`; keeps function
//! tools (`search_knowledge`).

use std::sync::Arc;

use futures::StreamExt;
use oagw_sdk::ServiceGatewayClientV1;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use serde_json::{Map, Value, json};
use toolkit_security::SecurityContext;

use super::{json_request, map_error_response, map_gateway_error, normalize_incomplete_reason, parse_usage};
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::{LlmProviderError, LlmRequest, ProviderEvent, ProviderEventStream, ToolSpec};

/// Build the Chat Completions request body.
#[must_use]
pub fn build_request_body(request: &LlmRequest) -> Vec<u8> {
    let mut messages: Vec<Value> = Vec::with_capacity(request.input.len() + 1);
    if !request.instructions.is_empty() {
        messages.push(json!({"role": "system", "content": request.instructions}));
    }
    for msg in &request.input {
        messages.push(json!({"role": msg.role, "content": msg.text}));
    }
    // Agentic-loop trailing items arrive as Responses-style items; convert
    // function calls and outputs to chat-completions tool messages.
    for item in &request.trailing_items {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or_default();
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or_default();
                messages.push(json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }],
                }));
            }
            Some("function_call_output") => {
                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or_default();
                let output = item.get("output").and_then(Value::as_str).unwrap_or_default();
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output,
                }));
            }
            _ => {}
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .filter_map(|tool| match tool {
            ToolSpec::Function {
                name,
                description,
                parameters,
            } => Some(json!({
                "type": "function",
                "function": {"name": name, "description": description, "parameters": parameters},
            })),
            _ => None,
        })
        .collect();
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::from(request.provider_model_id.clone()));
    body.insert("messages".to_owned(), Value::Array(messages));
    if !tools.is_empty() {
        body.insert("tools".to_owned(), Value::Array(tools));
        body.insert("tool_choice".to_owned(), json!("auto"));
    }
    body.insert(
        "max_completion_tokens".to_owned(),
        Value::from(request.max_output_tokens),
    );
    body.insert("user".to_owned(), Value::from(request.user.clone()));
    body.insert("stream".to_owned(), Value::from(request.stream));
    if request.stream {
        body.insert(
            "stream_options".to_owned(),
            json!({"include_usage": true}),
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
        body.insert("reasoning_effort".to_owned(), json!(effort));
    }
    if let Some(extra) = &params.extra_body {
        for (key, value) in extra {
            if super::openai_responses::CONTROLLED_KEYS.contains(&key.as_str()) {
                tracing::warn!(
                    "mini-chat: extra_body key '{key}' is controlled by the request; ignored"
                );
                continue;
            }
            body.insert(key.clone(), value.clone());
        }
    }
    serde_json::to_vec(&Value::Object(body)).unwrap_or_default()
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

/// Streaming state for one response.
#[derive(Default)]
struct StreamState {
    /// Accumulated tool calls by index.
    tool_calls: std::collections::BTreeMap<u64, (String, String, String)>,
    /// `Usage` from the final chunk.
    usage: Option<mini_chat_sdk::UsageTokens>,
    /// Response id.
    id: Option<String>,
    /// Events queued for emission.
    pending: std::collections::VecDeque<ProviderEvent>,
}

#[allow(clippy::redundant_continue, reason = "event loop is clearest with explicit continue")]
fn event_stream(
    stream: ServerEventsStream<ServerEvent>,
) -> impl futures::Stream<Item = Result<ProviderEvent, LlmProviderError>> {
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
                let data = event.data.trim().to_owned();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    let calls: Vec<(String, String, String)> = state
                        .tool_calls
                        .values()
                        .filter(|(name, _, _)| !name.is_empty())
                        .cloned()
                        .collect();
                    for (name, call_id, arguments) in calls {
                        state.pending.push_back(ProviderEvent::ToolDone {
                            name: "function_call".to_owned(),
                            details: json!({
                                "call_id": call_id,
                                "name": name,
                                "arguments": arguments,
                            }),
                        });
                        state.pending.push_back(ProviderEvent::FunctionCall {
                            name,
                            call_id,
                            arguments,
                        });
                    }
                    state.pending.push_back(ProviderEvent::Completed {
                        usage: state.usage,
                        provider_response_id: state.id.clone(),
                        incomplete_reason: None,
                    });
                    continue;
                }
                let value: Value = match serde_json::from_str(&data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if value.get("usage").is_some() {
                    state.usage = parse_usage(&value);
                }
                if state.id.is_none() {
                    state.id = value
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                if let Some(error) = value.get("error") {
                    let message = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("provider error");
                    return Some((
                        Ok(ProviderEvent::Failed {
                            code: "provider_error".to_owned(),
                            message: crate::infra::sanitize::sanitize_provider_message(message),
                            usage: state.usage,
                            provider_response_id: state.id.clone(),
                        }),
                        (stream, state),
                    ));
                }
                let Some(choice) = value
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                else {
                    continue;
                };
                // Tool call deltas.
                if let Some(tool_calls) = choice
                    .get("delta")
                    .and_then(|d| d.get("tool_calls"))
                    .and_then(Value::as_array)
                {
                    for call in tool_calls {
                        let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let entry = state.tool_calls.entry(index).or_default();
                        if let Some(id) = call.get("id").and_then(Value::as_str) {
                            entry.1 = id.to_owned();
                        }
                        if let Some(name) = call
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(Value::as_str)
                        {
                            entry.0 = name.to_owned();
                            state.pending.push_back(ProviderEvent::ToolStart {
                                name: "function_call".to_owned(),
                                details: json!({
                                    "index": index,
                                    "call_id": entry.1,
                                    "name": name,
                                }),
                            });
                        }
                        if let Some(args) = call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                        {
                            entry.2.push_str(args);
                        }
                    }
                }
                // Text delta.
                if let Some(text) = choice
                    .get("delta")
                    .and_then(|d| d.get("content"))
                    .and_then(Value::as_str)
                    && !text.is_empty()
                {
                    return Some((Ok(ProviderEvent::TextDelta(text.to_owned())), (stream, state)));
                }
                // Finish reason.
                if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
                    let incomplete = match finish {
                        "stop" | "tool_calls" => None,
                        other => normalize_incomplete_reason(Some(other)),
                    };
                    state.pending.push_back(ProviderEvent::Completed {
                        usage: state.usage,
                        provider_response_id: state.id.clone(),
                        incomplete_reason: incomplete,
                    });
                    continue;
                }
            }
        },
    )
}
