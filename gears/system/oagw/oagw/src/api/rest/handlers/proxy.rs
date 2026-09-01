// Created: 2026-08-29 by Constructor Tech
//! Data-plane REST handler: the proxy endpoint for HTTP, SSE and WebSocket.
//!
//! Security: the handler logs nothing about the request; the problem document
//! carries only the status, the GTS error type, the correlation id and the
//! upstream routing context.

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request};
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite;

use futures_util::Stream;

use super::{CtxExtension, Services, calling_security};
use crate::api::rest::error::{ApiError, trace_id_from_headers};
use crate::domain::error::{OagwError, error_type};
use crate::domain::model::MAX_BODY_BYTES;
use crate::infra::proxy::headers;
use crate::infra::proxy::service::{ProxyBody, ProxyOutcome, ProxyRequest, WebSocketLeg};

/// Path parameters of the proxy route.
pub(crate) struct ProxyPath {
    /// Routing alias.
    pub alias: String,
    /// Raw remainder of the path after the alias.
    pub path_suffix: String,
}

/// `any /oagw/v1/proxy/{alias}`.
pub async fn proxy(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path(alias): Path<String>,
    uri: Uri,
    request: Request,
) -> Response {
    serve(
        services,
        ctx,
        ProxyPath {
            alias,
            path_suffix: String::new(),
        },
        uri,
        request,
    )
    .await
}

/// `any /oagw/v1/proxy/{alias}/{*path_suffix}`.
pub async fn proxy_with_suffix(
    Extension(services): Extension<Arc<Services>>,
    ctx: CtxExtension,
    Path((alias, suffix)): Path<(String, String)>,
    uri: Uri,
    request: Request,
) -> Response {
    serve(
        services,
        ctx,
        ProxyPath {
            alias,
            path_suffix: suffix,
        },
        uri,
        request,
    )
    .await
}

async fn serve(
    services: Arc<Services>,
    ctx: CtxExtension,
    path: ProxyPath,
    uri: Uri,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    if is_websocket(&parts.method, &parts.headers) {
        return upgrade(services, ctx, path, uri, parts).await;
    }
    let buffered = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return problem(OagwError::PayloadTooLarge, uri.path(), None),
    };
    if let Some(error) = validate_body(&parts.headers, buffered.len()) {
        return problem(
            error,
            uri.path(),
            Some(trace_id_from_headers(&parts.headers).as_str()),
        );
    }
    let proxy_request = build(path, &uri, parts.method, parts.headers, buffered, ctx);
    let trace_id = proxy_request.trace_id.clone();
    let instance = proxy_request.instance.clone();
    match services.data_plane.handle(proxy_request).await {
        Ok(outcome) => outcome_response(outcome),
        Err(failure) => failure_response(failure, &instance, &trace_id),
    }
}

/// `true` for a WebSocket upgrade request.
fn is_websocket(method: &axum::http::Method, headers: &axum::http::HeaderMap) -> bool {
    if *method != axum::http::Method::GET {
        return false;
    }
    let connection = headers
        .get(axum::http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"));
    let upgrade = headers
        .get(axum::http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    connection && upgrade && headers.contains_key(axum::http::header::SEC_WEBSOCKET_KEY)
}

/// Body validation before forwarding.
///
/// # Errors
///
/// Returns the wire error for a `Content-Length` mismatch or a non-chunked
/// `Transfer-Encoding`.
fn validate_body(headers: &axum::http::HeaderMap, actual: usize) -> Option<OagwError> {
    if let Some(value) = headers.get(axum::http::header::CONTENT_LENGTH)
        && let Ok(text) = value.to_str()
        && let Ok(declared) = text.trim().parse::<usize>()
        && declared != actual
    {
        return Some(OagwError::Validation(
            "content-length does not match the request body".to_owned(),
        ));
    }
    if let Some(value) = headers.get(axum::http::header::TRANSFER_ENCODING)
        && let Ok(text) = value.to_str()
        && !text.trim().eq_ignore_ascii_case("chunked")
    {
        return Some(OagwError::Validation(
            "transfer-encoding is only supported as chunked".to_owned(),
        ));
    }
    None
}

/// Build the transport-neutral proxy request.
fn build(
    path: ProxyPath,
    uri: &Uri,
    method: axum::http::Method,
    headers: axum::http::HeaderMap,
    body: Bytes,
    ctx: CtxExtension,
) -> ProxyRequest {
    let security = calling_security(&ctx);
    let trace_id = trace_id_from_headers(&headers);
    ProxyRequest {
        method: method.as_str().to_owned(),
        alias: path.alias,
        path_suffix: path.path_suffix,
        query: uri.query().map(str::to_owned),
        headers,
        body,
        tenant_id: security.subject_tenant_id(),
        subject_id: security.subject_id(),
        client_ip: String::new(),
        instance: uri.path().to_owned(),
        trace_id,
        security,
        route_pattern: None,
    }
}

/// Turn a resolved outcome into the wire response.
fn outcome_response(outcome: ProxyOutcome) -> Response {
    let mut builder = Response::builder().status(outcome.status);
    for (name, value) in &outcome.headers {
        builder = builder.header(name, value);
    }
    match outcome.body {
        ProxyBody::Full(bytes) => builder.body(Body::from(bytes)),
        ProxyBody::Stream(stream) => builder.body(Body::from_stream(relay_stream(stream))),
    }
    .unwrap_or_else(|error| problem(OagwError::Internal(error.to_string()), "", None))
}

/// Relay an upstream byte stream to the client.
///
/// A mid-stream failure cannot change the status line (headers are already
/// sent), so it is reported as a final `event: error` frame carrying the GTS
/// error type and the stream then ends. The frame names the error type only:
/// no request, response or credential material is ever included.
fn relay_stream<S>(stream: S) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: Stream<Item = Result<Bytes, OagwError>> + Send + Unpin + 'static,
{
    futures_util::stream::unfold((stream, false), |(mut stream, aborted)| async move {
        if aborted {
            return None;
        }
        match StreamExt::next(&mut stream).await {
            Some(Ok(bytes)) => Some((Ok(bytes), (stream, false))),
            Some(Err(error)) => {
                let frame = format!(
                    "event: error\ndata: {}\n\n",
                    error_type(error.type_suffix())
                );
                Some((Ok(Bytes::from(frame)), (stream, true)))
            }
            None => None,
        }
    })
}

/// RFC 9457 problem response for a gateway error.
fn problem(error: OagwError, instance: &str, trace_id: Option<&str>) -> Response {
    let mut api = ApiError::new(error).with_instance(instance);
    if let Some(trace_id) = trace_id {
        api = api.with_trace_id(trace_id);
    }
    api.into_response()
}

/// Gateway failure carrying the upstream routing context.
fn failure_response(
    failure: crate::infra::proxy::service::ProxyFailure,
    instance: &str,
    trace_id: &str,
) -> Response {
    let mut api = ApiError::new(failure.kind)
        .with_instance(instance)
        .with_trace_id(trace_id);
    api.context.upstream_id = failure.upstream_id.map(|id| id.to_string());
    api.context.host = failure.host;
    let mut response = api.into_response();
    // A rejected request still reports the budget it would have spent
    // (ADR-0003), so the counters travel on the 429 next to `Retry-After`.
    if let Some(report) = failure.rate.as_ref() {
        let headers = response.headers_mut();
        for (name, value) in [
            ("x-ratelimit-limit", report.limit.to_string()),
            ("x-ratelimit-remaining", report.remaining.to_string()),
            ("x-ratelimit-reset", report.reset_epoch.to_string()),
        ] {
            if let (Ok(name), Some(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                headers::header_value(&value),
            ) {
                headers.insert(name, value);
            }
        }
    }
    response
}

// ---------------------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------------------

async fn upgrade(
    services: Arc<Services>,
    ctx: CtxExtension,
    path: ProxyPath,
    uri: Uri,
    mut parts: axum::http::request::Parts,
) -> Response {
    let headers = parts.headers.clone();
    let Ok(upgrade) = WebSocketUpgrade::from_request_parts(&mut parts, &()).await else {
        return problem(
            OagwError::Validation("request is not a websocket upgrade".to_owned()),
            uri.path(),
            None,
        );
    };
    let proxy_request = build(path, &uri, parts.method, headers, Bytes::new(), ctx);
    let trace_id = proxy_request.trace_id.clone();
    let instance = proxy_request.instance.clone();
    let leg = match services
        .data_plane
        .resolve_for_websocket(proxy_request)
        .await
    {
        Ok(leg) => leg,
        Err(failure) => return failure_response(failure, &instance, &trace_id),
    };
    let upstream = match connect_upstream(&leg).await {
        Ok(stream) => stream,
        Err(error) => {
            services.data_plane.websocket_failure(&leg);
            let mut api = ApiError::new(error)
                .with_instance(instance)
                .with_trace_id(trace_id);
            api.context.upstream_id = Some(leg.upstream_id.to_string());
            api.context.host = Some(leg.host);
            return api.into_response();
        }
    };
    services.data_plane.websocket_success(&leg);
    upgrade.on_upgrade(move |socket: WebSocket| async move {
        relay(socket, upstream).await;
    })
}

/// Connect the upstream WebSocket leg.
///
/// `wss://` requires a TLS backend the MVP build does not enable, so such
/// targets fail closed with `502 DownstreamError`.
async fn connect_upstream(
    leg: &WebSocketLeg,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    OagwError,
> {
    let mut builder = http::Request::builder().uri(leg.url.clone());
    for (name, value) in &leg.headers {
        if is_handshake_header(name) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    // The handshake material is always freshly minted for the upstream leg:
    // the caller's key is never replayed and the authority is the upstream's.
    let authority = http::Uri::try_from(leg.url.as_str())
        .ok()
        .and_then(|uri| uri.authority().map(|value| value.as_str().to_owned()))
        .unwrap_or_default();
    builder = builder
        .header("host", authority)
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header(
            "sec-websocket-key",
            tungstenite::handshake::client::generate_key(),
        );
    let request = builder
        .body(())
        .map_err(|_| OagwError::ProtocolError("websocket target is not a valid URL".to_owned()))?;
    match tokio_tungstenite::connect_async(request).await {
        Ok((stream, _response)) => Ok(stream),
        Err(_) => Err(OagwError::DownstreamError(
            "upstream websocket handshake failed".to_owned(),
        )),
    }
}

/// Headers the tunneling library computes itself.
fn is_handshake_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-extensions"
            | "sec-websocket-protocol"
    )
}

/// Relay frames in both directions until either side closes.
async fn relay(
    client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut client_sink, mut client_stream) = client.split();
    let (mut upstream_sink, mut upstream_stream) = upstream.split();
    let outbound = async move {
        while let Some(Ok(message)) = client_stream.next().await {
            let Some(frame) = to_upstream_frame(message) else {
                break;
            };
            if upstream_sink.send(frame).await.is_err() {
                break;
            }
        }
        let _ = upstream_sink.close().await;
    };
    let inbound = async move {
        while let Some(Ok(message)) = upstream_stream.next().await {
            let Some(frame) = to_client_frame(message) else {
                break;
            };
            if client_sink.send(frame).await.is_err() {
                break;
            }
        }
        let _ = client_sink.close().await;
    };
    tokio::join!(outbound, inbound);
}

/// Translate an axum frame into a tungstenite frame.
fn to_upstream_frame(message: Message) -> Option<tungstenite::Message> {
    match message {
        Message::Text(text) => Some(tungstenite::Message::text(text.as_str())),
        Message::Binary(data) => Some(tungstenite::Message::binary(data)),
        Message::Ping(data) => Some(tungstenite::Message::Ping(data)),
        Message::Pong(data) => Some(tungstenite::Message::Pong(data)),
        Message::Close(_) => Some(tungstenite::Message::Close(None)),
    }
}

/// Translate a tungstenite frame into an axum frame.
fn to_client_frame(message: tungstenite::Message) -> Option<Message> {
    match message {
        tungstenite::Message::Text(text) => Some(Message::text(text.as_str())),
        tungstenite::Message::Binary(data) => Some(Message::binary(data)),
        tungstenite::Message::Ping(data) => Some(Message::Ping(data)),
        tungstenite::Message::Pong(data) => Some(Message::Pong(data)),
        tungstenite::Message::Close(_) | tungstenite::Message::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upgrade_headers() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONNECTION,
            axum::http::HeaderValue::from_static("Upgrade"),
        );
        headers.insert(
            axum::http::header::UPGRADE,
            axum::http::HeaderValue::from_static("websocket"),
        );
        headers.insert(
            axum::http::header::SEC_WEBSOCKET_KEY,
            axum::http::HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers
    }

    #[test]
    fn websocket_upgrade_is_detected_from_headers() {
        assert!(is_websocket(&axum::http::Method::GET, &upgrade_headers()));
        let mut incomplete = upgrade_headers();
        incomplete.remove(axum::http::header::SEC_WEBSOCKET_KEY);
        assert!(!is_websocket(&axum::http::Method::GET, &incomplete));
        assert!(!is_websocket(&axum::http::Method::POST, &upgrade_headers()));
    }

    #[test]
    fn message_translation_is_lossless_for_data_frames() {
        let text = Message::text("hello");
        assert_eq!(
            to_upstream_frame(text.clone()),
            Some(tungstenite::Message::text("hello"))
        );
        assert_eq!(
            to_client_frame(tungstenite::Message::text("hello")),
            Some(text)
        );
        let binary = Message::binary(Bytes::from_static(b"\x01\x02"));
        assert_eq!(
            to_upstream_frame(binary.clone()),
            Some(tungstenite::Message::binary(Bytes::from_static(&[
                0x01, 0x02
            ])))
        );
        assert_eq!(
            to_client_frame(tungstenite::Message::binary(Bytes::from_static(&[
                0x01, 0x02
            ]))),
            Some(binary)
        );
    }

    #[test]
    fn control_frames_map_to_close() {
        assert!(to_upstream_frame(Message::Close(None)).is_some());
        assert!(to_upstream_frame(Message::Ping(Bytes::new())).is_some());
        assert!(to_client_frame(tungstenite::Message::Close(None)).is_none());
        assert!(
            to_client_frame(tungstenite::Message::Frame(
                tungstenite::protocol::frame::Frame::ping(Bytes::new())
            ))
            .is_none()
        );
    }

    #[test]
    fn handshake_headers_are_not_forwarded() {
        assert!(is_handshake_header("Connection"));
        assert!(is_handshake_header("SEC-WEBSOCKET-KEY"));
        assert!(!is_handshake_header("authorization"));
    }

    #[test]
    fn body_validation_accepts_matching_length() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_LENGTH,
            axum::http::HeaderValue::from_static("3"),
        );
        assert!(validate_body(&headers, 3).is_none());
        assert!(validate_body(&headers, 4).is_some());
        headers.remove(axum::http::header::CONTENT_LENGTH);
        assert!(validate_body(&headers, 0).is_none());
    }

    #[test]
    fn transfer_encoding_must_be_chunked() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::TRANSFER_ENCODING,
            axum::http::HeaderValue::from_static("gzip"),
        );
        assert!(validate_body(&headers, 0).is_some());
        headers.insert(
            axum::http::header::TRANSFER_ENCODING,
            axum::http::HeaderValue::from_static("chunked"),
        );
        assert!(validate_body(&headers, 0).is_none());
    }

    #[test]
    fn body_limit_is_100_mib() {
        assert_eq!(MAX_BODY_BYTES, 100 * 1024 * 1024);
    }
}
