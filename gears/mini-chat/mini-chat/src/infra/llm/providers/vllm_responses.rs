//! `vLLM` `Responses` API adapter.
//!
//! Drops all tools (including function tools) and does not send
//! `max_tool_calls` or `metadata`. Text inside `<think>` blocks is mapped to
//! reasoning deltas.

use std::sync::Arc;

use futures::StreamExt;
use oagw_sdk::ServiceGatewayClientV1;
use toolkit_security::SecurityContext;

use super::{map_error_response, map_gateway_error};
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::{LlmProviderError, LlmRequest, ProviderEvent, ProviderEventStream};

/// Send a streaming chat request.
pub async fn stream_chat(
    gateway: &Arc<dyn ServiceGatewayClientV1>,
    ctx: &SecurityContext,
    provider: &ResolvedProvider,
    request: &LlmRequest,
) -> Result<ProviderEventStream, LlmProviderError> {
    // Reuse the Responses request shape without tools or metadata.
    let mut request = request.clone();
    request.tools = Vec::new();
    request.metadata = None;
    let body = super::openai_responses::build_request_body(&request);
    // Remove max_tool_calls (not sent by this adapter).
    let mut value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| LlmProviderError::InvalidResponse(format!("request build failed: {e}")))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("max_tool_calls");
        obj.remove("tools");
        obj.remove("include");
    }
    let body = serde_json::to_vec(&value).unwrap_or_default();
    let path = provider
        .api_path
        .replace("{model}", &request.provider_model_id);
    let req = super::json_request(provider, &path, body)?;
    let resp = gateway
        .proxy_request(ctx.clone(), req)
        .await
        .map_err(map_gateway_error)?;
    match oagw_sdk::sse::ServerEventsStream::from_response::<oagw_sdk::sse::ServerEvent>(resp) {
        oagw_sdk::sse::ServerEventsResponse::Events(stream) => {
            Ok(Box::pin(think_split(super::openai_responses::event_stream(stream))))
        }
        oagw_sdk::sse::ServerEventsResponse::Response(resp) => Err(map_error_response(resp).await),
    }
}

/// Split `<think>` blocks out of text deltas into reasoning deltas.
fn think_split<S>(
    stream: S,
) -> impl futures::Stream<Item = Result<ProviderEvent, LlmProviderError>> + Send
where
    S: futures::Stream<Item = Result<ProviderEvent, LlmProviderError>> + Send,
{
    struct State<S> {
        stream: std::pin::Pin<Box<S>>,
        in_think: bool,
        hold: String,
        pending: std::collections::VecDeque<Result<ProviderEvent, LlmProviderError>>,
    }
    futures::stream::unfold(
        State {
            stream: Box::pin(stream),
            in_think: false,
            hold: String::new(),
            pending: std::collections::VecDeque::new(),
        },
        |mut st| async move {
            loop {
                if let Some(event) = st.pending.pop_front() {
                    return Some((event, st));
                }
                let item = st.stream.next().await?;
                match item {
                    Ok(ProviderEvent::TextDelta(text)) => {
                        let buf = std::mem::take(&mut st.hold) + &text;
                        for event in split_delta(&mut st.in_think, &mut st.hold, buf) {
                            st.pending.push_back(Ok(event));
                        }
                    }
                    other => st.pending.push_back(other),
                }
            }
        },
    )
}

/// Split one text delta into text/reasoning deltas, holding back a trailing
/// partial tag for the next delta.
fn split_delta(in_think: &mut bool, hold: &mut String, buf: String) -> Vec<ProviderEvent> {
    let mut out = Vec::new();
    let mut rest = buf.as_str();
    loop {
        let tag = if *in_think { "</think>" } else { "<think>" };
        match rest.find(tag) {
            Some(pos) => {
                push(&mut out, *in_think, &rest[..pos]);
                *in_think = !*in_think;
                rest = &rest[pos + tag.len()..];
            }
            None => {
                // Hold back a trailing partial tag candidate.
                if let Some(idx) = rest.rfind('<') {
                    let tail = &rest[idx..];
                    if tag.starts_with(tail) {
                        *hold = tail.to_owned();
                        push(&mut out, *in_think, &rest[..idx]);
                        return out;
                    }
                }
                push(&mut out, *in_think, rest);
                return out;
            }
        }
    }
}

fn push(out: &mut Vec<ProviderEvent>, in_think: bool, text: &str) {
    if text.is_empty() {
        return;
    }
    if in_think {
        out.push(ProviderEvent::ReasoningDelta(text.to_owned()));
    } else {
        out.push(ProviderEvent::TextDelta(text.to_owned()));
    }
}
