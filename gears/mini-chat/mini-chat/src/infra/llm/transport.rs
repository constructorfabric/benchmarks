//! OAGW transport shared by the chat adapters: a JSON `POST` with the S2S context, HTTP error
//! mapping, and the translation of a provider SSE stream into [`ProviderEvent`]s as the events
//! arrive (nothing is buffered).

use std::sync::Arc;

use futures::StreamExt;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::Value;

use super::errors;
use super::types::{ProviderError, ProviderEvent, ProviderEventStream};
use super::{ChatTarget, S2sContext};

/// `POST {target uri}` with a JSON body; `stream` adds `Accept: text/event-stream`. A non-2xx
/// response is mapped to a [`ProviderError`].
pub(super) async fn post_json(
    gateway: &Arc<dyn ServiceGatewayClientV1>,
    s2s: &S2sContext,
    target: &ChatTarget,
    model: &str,
    body: &Value,
    stream: bool,
    headers: &[(&str, &str)],
) -> Result<http::Response<Body>, ProviderError> {
    let ctx = s2s.get().map_err(|e| errors::from_s2s(&e))?;
    let payload = serde_json::to_vec(body)
        .map_err(|_| ProviderError::provider("failed to encode the provider request"))?;
    let mut builder = http::Request::builder()
        .method(http::Method::POST)
        .uri(target.uri_for(model))
        .header(http::header::CONTENT_TYPE, "application/json");
    if stream {
        builder = builder.header(http::header::ACCEPT, "text/event-stream");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let http_req = builder
        .body(Body::from(payload))
        .map_err(|_| ProviderError::provider("failed to build the provider request"))?;
    let resp = gateway
        .proxy_request(ctx, http_req)
        .await
        .map_err(|e| errors::from_canonical(&e))?;
    if resp.status().is_success() {
        return Ok(resp);
    }
    let source = resp.extensions().get::<ErrorSource>().copied();
    let (parts, body) = resp.into_parts();
    let bytes = match body.into_bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(status = parts.status.as_u16(), cause = %errors::loggable(&e),
                "failed to read the body of a provider error response");
            bytes::Bytes::new()
        }
    };
    Err(errors::from_http(
        source,
        parts.status,
        &parts.headers,
        &bytes,
    ))
}

/// The body of a successful non-streaming call, as JSON.
pub(super) async fn json_body(resp: http::Response<Body>) -> Result<Value, ProviderError> {
    let bytes = resp.into_body().into_bytes().await.map_err(|e| {
        tracing::warn!(cause = %errors::loggable(&e), "failed to read the provider response");
        ProviderError::provider("failed to read the provider response")
    })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        tracing::warn!(cause = %e, "provider response is not valid JSON");
        ProviderError::provider("provider returned an invalid response")
    })
}

/// The SSE events of a successful streaming call.
pub(super) fn event_stream(
    resp: http::Response<Body>,
) -> Result<ServerEventsStream<ServerEvent>, ProviderError> {
    match ServerEventsStream::from_response::<ServerEvent>(resp) {
        ServerEventsResponse::Events(events) => Ok(events),
        ServerEventsResponse::Response(_) => Err(ProviderError::provider(
            "provider did not return an event stream",
        )),
    }
}

/// Translates one wire event into zero or more provider events; [`Translate::finish`] runs when
/// the wire stream ends without a terminal event.
pub(super) trait Translate: Send + 'static {
    fn translate(&mut self, raw: &ServerEvent) -> Vec<ProviderEvent>;

    /// Events for a wire stream that ended without a terminal event (default: a failure).
    fn finish(&mut self) -> Vec<ProviderEvent> {
        vec![ProviderEvent::Failed(ProviderError::provider(
            "provider stream ended without a terminal event",
        ))]
    }
}

struct StreamState<T> {
    events: ServerEventsStream<ServerEvent>,
    translator: T,
    queue: std::collections::VecDeque<ProviderEvent>,
    done: bool,
}

/// Yields the translated events in wire order; the stream ends after the first terminal event.
pub(super) fn translate_stream<T: Translate>(
    events: ServerEventsStream<ServerEvent>,
    translator: T,
) -> ProviderEventStream {
    let state = StreamState {
        events,
        translator,
        queue: std::collections::VecDeque::new(),
        done: false,
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        loop {
            if state.done {
                return None;
            }
            if let Some(event) = state.queue.pop_front() {
                state.done = event.is_terminal();
                return Some((event, state));
            }
            match state.events.next().await {
                Some(Ok(raw)) => {
                    let out = state.translator.translate(&raw);
                    state.queue.extend(out);
                }
                Some(Err(e)) => {
                    tracing::warn!(cause = %errors::loggable(&e),
                        "provider stream was interrupted by a transport error");
                    state
                        .queue
                        .push_back(ProviderEvent::Failed(ProviderError::provider(
                            "provider stream was interrupted",
                        )));
                }
                None => {
                    let mut out = state.translator.finish();
                    if !out.last().is_some_and(ProviderEvent::is_terminal) {
                        out.push(ProviderEvent::Failed(ProviderError::provider(
                            "provider stream ended without a terminal event",
                        )));
                    }
                    state.queue.extend(out);
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use http::Method;
    use serde_json::json;
    use toolkit_canonical_errors::CanonicalError;

    use super::*;
    use crate::config::ProviderKind;
    use crate::test_support::authn::s2s_security_context;
    use crate::test_support::fixtures::chat_target;
    use crate::test_support::gateway::{FakeGateway, Responder, SseScript};

    const PATH: &str = "/v1/responses";
    /// A credential that must never reach the logs.
    const SECRET: &str = "sk-abcdefghijklmnopqrstuvwx";

    struct Ignore;

    impl Translate for Ignore {
        fn translate(&mut self, _raw: &ServerEvent) -> Vec<ProviderEvent> {
            Vec::new()
        }
    }

    fn setup(responder: Responder) -> (Arc<dyn ServiceGatewayClientV1>, S2sContext, ChatTarget) {
        let gateway = FakeGateway::new();
        gateway.on(Method::POST, PATH, responder);
        let s2s = S2sContext::new();
        s2s.set(s2s_security_context());
        (
            Arc::new(gateway),
            s2s,
            chat_target(ProviderKind::OpenaiResponses),
        )
    }

    async fn post(
        responder: Responder,
        stream: bool,
    ) -> Result<http::Response<Body>, ProviderError> {
        let (gateway, s2s, target) = setup(responder);
        post_json(&gateway, &s2s, &target, "m", &json!({}), stream, &[]).await
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn gateway_failure_cause_is_logged_without_credentials() {
        let cause = format!("connect refused (Authorization: Bearer {SECRET})");
        let err = post(
            Responder::Err(CanonicalError::internal(cause).create()),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(err.message, "provider request failed");
        assert!(logs_contain("connect refused"));
        assert!(!logs_contain(SECRET));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn gateway_error_response_is_logged() {
        let err = post(Responder::GatewayStatus(502), true).await.unwrap_err();
        assert_eq!(err.message, "provider gateway error (502)");
        assert!(logs_contain("502"));
        assert!(logs_contain("gateway error"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn error_body_read_failure_is_logged() {
        let err = post(
            Responder::BodyError(500, "connection reset".to_owned()),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(err.message, "provider returned HTTP 500");
        assert!(logs_contain("connection reset"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn success_body_read_failure_is_logged() {
        let resp = post(
            Responder::BodyError(200, "body truncated".to_owned()),
            false,
        )
        .await
        .unwrap();
        let err = json_body(resp).await.unwrap_err();
        assert_eq!(err.message, "failed to read the provider response");
        assert!(logs_contain("body truncated"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn mid_stream_transport_error_is_logged() {
        let resp = post(
            Responder::Sse(vec![
                SseScript::event("ping", json!({})),
                SseScript::Fail(format!("reset by peer, Bearer {SECRET}")),
            ]),
            true,
        )
        .await
        .unwrap();
        let events: Vec<ProviderEvent> = translate_stream(event_stream(resp).unwrap(), Ignore)
            .collect()
            .await;
        assert!(
            matches!(events.as_slice(), [ProviderEvent::Failed(e)] if e.message == "provider stream was interrupted"),
            "{events:?}"
        );
        assert!(logs_contain("reset by peer"));
        assert!(!logs_contain(SECRET));
    }
}
