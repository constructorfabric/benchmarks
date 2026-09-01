//! Proxy data-plane handlers (`/api/oagw/v1/proxy/{alias}`, DESIGN §3.5).
//!
//! Three operations live here:
//!
//! | Request | Behaviour |
//! |---|---|
//! | `OPTIONS /proxy/{alias}/…` | CORS preflight, permissive 204 (ADR 0004) |
//! | `ANY /proxy/{alias}/{*suffix}` | buffered request, streamed response |
//! | upgrade request | inbound axum upgrade tunnelled to the upstream (A1) |
//!
//! Plus `GET /api/oagw/v1/metrics`, the Prometheus exposition of the same
//! engine (DESIGN §4.2).
//!
//! The handler never inspects, logs or echoes request data: the body, the
//! query string and every header value are handed to the engine as opaque
//! bytes and dropped as soon as the response is produced.
//!
//! Review evidence (privilege boundary — transport):
//! * Guardrail: DESIGN §3.5 "Proxy Request Flow" + §4.3 "No PII", ADR 0004
//!   (permissive CORS), ADR 0007 (error-source distinction).
//! * Rationale: this module is the only place that sees the raw HTTP request,
//!   so it must convert it into the engine's [`ProxyRequest`] without
//!   materialising anything a log line or an error body could capture.
//! * Validation performed: `proxy_tests` drives the registered router with
//!   `tower::ServiceExt::oneshot` against real upstream servers (plain HTTP,
//!   `text/event-stream` pass-through and a WebSocket upgrade).

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use toolkit_security::context::SecurityContext;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::Request as UpgradeRequest;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::api::rest::error;
use crate::domain::error::DomainError;
use crate::infra::proxy::{HttpProxyEngine, ProxyOutcome, ProxyRequest};

/// Prefix every proxied path starts with.
pub const PROXY_PREFIX: &str = "/api/oagw/v1/proxy";
/// Metrics endpoint of the data plane (DESIGN §4.2).
pub const METRICS_PATH: &str = "/api/oagw/v1/metrics";
/// Wire value of the `Upgrade` request header for a WebSocket.
pub const UPGRADE_VALUE: &str = "websocket";
/// `Access-Control-Max-Age` of the permissive preflight answer (ADR 0004).
pub const PREFLIGHT_MAX_AGE: &str = "86400";
/// Default `Access-Control-Allow-Headers` of a preflight answer.
pub const PREFLIGHT_DEFAULT_HEADERS: &str = "authorization, content-type";
/// `Vary` of a preflight answer, so caches never replay one origin to another.
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";
/// Hard ceiling on a buffered request body, in bytes. The configured
/// `max_payload_bytes` is enforced by the engine; this ceiling only bounds the
/// memory the transport is willing to hold.
const MAX_BUFFERED_BODY: usize = 256 * 1024 * 1024;

/// Engine extension type shared by the proxy and metrics handlers.
pub type Engine = Arc<HttpProxyEngine>;

/// Outbound WebSocket connection to a resolved upstream (A1).
type UpstreamSocket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

// ---------------------------------------------------------------------------
// path helpers
// ---------------------------------------------------------------------------

/// Path below the alias, as carried by the request URI.
///
/// `/api/oagw/v1/proxy/chat/v1/things` → `/v1/things`; a request addressed at
/// the alias root yields `/`, which is what `path_suffix_mode: append`
/// concatenates onto the configured route path.
#[must_use]
pub fn path_suffix(uri: &Uri) -> String {
    let trimmed = uri
        .path()
        .strip_prefix(PROXY_PREFIX)
        .unwrap_or(uri.path())
        .trim_start_matches('/');
    match trimmed.split_once('/') {
        Some((_, suffix)) => format!("/{suffix}"),
        None => "/".to_owned(),
    }
}

/// Alias the request was addressed to.
#[must_use]
pub fn alias_of(uri: &Uri) -> String {
    let trimmed = uri
        .path()
        .strip_prefix(PROXY_PREFIX)
        .unwrap_or(uri.path())
        .trim_start_matches('/');
    trimmed
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------
// inbound extraction
// ---------------------------------------------------------------------------

/// Everything the handler extracted from the inbound request.
struct Inbound {
    /// Negotiated inbound upgrade, when the request asked for one.
    upgrade: Option<WebSocketUpgrade>,
    /// Request the engine consumes.
    request: ProxyRequest,
}

/// Buffers the request into the shape the engine consumes.
///
/// The headers are lowered to lossy UTF-8 pairs in arrival order: the engine
/// works on plain strings, and a non-UTF-8 header value must not abort a
/// request that is only being relayed.
async fn inbound(
    request: axum::extract::Request,
    security: SecurityContext,
) -> Result<Inbound, DomainError> {
    let (mut parts, body) = request.into_parts();
    let headers = header_pairs(&parts.headers);
    let upgrade = upgrade_of(&mut parts, &headers).await?;
    let method = parts.method.to_string();
    let query = parts.uri.query().map(str::to_owned);
    let alias = alias_of(&parts.uri);
    let suffix = path_suffix(&parts.uri);

    let payload = axum::body::to_bytes(body, MAX_BUFFERED_BODY).await.map_err(|_| {
        DomainError::PayloadTooLarge {
            limit_bytes: u64::try_from(MAX_BUFFERED_BODY).unwrap_or(u64::MAX),
        }
    })?;

    Ok(Inbound {
        upgrade,
        request: ProxyRequest {
            alias,
            method,
            path: suffix,
            query,
            headers,
            body: payload,
            security,
        },
    })
}

/// Negotiates the inbound upgrade, when the request asked for one.
///
/// A request that *looks* like an upgrade but fails the RFC 6455 handshake
/// check (`Sec-WebSocket-Key`, version 13, HTTP/1.1) is rejected here rather
/// than forwarded: relaying it would leave the caller waiting on a 101 that
/// never arrives.
async fn upgrade_of(
    parts: &mut http::request::Parts,
    headers: &[(String, String)],
) -> Result<Option<WebSocketUpgrade>, DomainError> {
    if !is_upgrade_request(headers) {
        return Ok(None);
    }
    let upgrade = WebSocketUpgrade::from_request_parts(parts, &())
        .await
        .map_err(|_| DomainError::ProtocolError {
            detail: "the WebSocket upgrade request is not a valid RFC 6455 handshake".to_owned(),
            upstream_id: None,
            host: None,
        })?;
    Ok(Some(upgrade))
}

/// Header values as lossy UTF-8 pairs, lower-cased, in arrival order.
fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// First value of `name`, case-insensitively.
fn header_of<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Whether the request carries `Upgrade: websocket` together with
/// `Connection: upgrade` (RFC 6455 §4.1).
fn is_upgrade_request(headers: &[(String, String)]) -> bool {
    header_of(headers, "upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case(UPGRADE_VALUE))
        && header_of(headers, "connection")
            .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"))
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// `ANY /proxy/{alias}[/{path_suffix}]` — the data-plane entry point.
///
/// `OPTIONS` never reaches the upstream: the gateway answers the preflight
/// itself so a browser can discover the policy (ADR 0004). The preflight also
/// runs *before* the caller's identity is resolved, because browsers send
/// preflights without credentials. An upgrade request is tunnelled (A1);
/// everything else goes through the buffered pipeline.
///
/// # Errors
///
/// Never returns `Err`. A gateway failure is rendered as the RFC 9457 problem
/// document with `X-OAGW-Error-Source: gateway`; an upstream response —
/// including an error status — is passed through with
/// `X-OAGW-Error-Source: upstream` (ADR 0007).
pub async fn proxy(
    Extension(engine): Extension<Engine>,
    security: Option<Extension<SecurityContext>>,
    request: axum::extract::Request,
) -> Response {
    if request.method() == Method::OPTIONS {
        return preflight(request.headers());
    }

    let uri = request.uri().clone();
    let Some(Extension(security)) = security else {
        return error::into_response(
            &DomainError::Internal {
                detail: "the request carries no security context".to_owned(),
            },
            Some(&uri.to_string()),
            None,
        );
    };

    let inbound = match inbound(request, security).await {
        Ok(inbound) => inbound,
        Err(failure) => return error::into_response(&failure, Some(&uri.to_string()), None),
    };

    if let Some(upgrade) = inbound.upgrade {
        return match tunnel(&engine, &inbound.request, upgrade).await {
            Ok(response) => response,
            Err(failure) => error::into_response(&failure, Some(&uri.to_string()), None),
        };
    }

    match engine.execute(inbound.request).await {
        Ok(outcome) => proxied_response(outcome),
        Err(failure) => error::into_response(&failure, Some(&uri.to_string()), None),
    }
}

/// Renders an upstream response, streaming its body through unchanged.
///
/// A header the wire rejects is dropped rather than failing the whole
/// response: the upstream already answered, and the caller is better served by
/// the remaining headers than by a synthetic 502.
#[must_use]
pub fn proxied_response(outcome: ProxyOutcome) -> Response {
    let status = StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in outcome.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(&value),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(Body::new(outcome.body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// `OPTIONS /proxy/{alias}[/{path_suffix}]` — permissive preflight (ADR 0004).
///
/// The answer mirrors the requested origin, method and headers and lets the
/// browser cache it for [`PREFLIGHT_MAX_AGE`]; a request without an `Origin`
/// gets the `Vary`-only shape so a non-browser caller is not told a CORS
/// policy exists.
#[must_use]
pub fn preflight(headers: &HeaderMap) -> Response {
    let origin = header_text(headers, "origin");
    let method = header_text(headers, "access-control-request-method");
    let requested = header_text(headers, "access-control-request-headers");

    let mut builder = Response::builder().status(StatusCode::NO_CONTENT);
    if let Some(origin) = origin.filter(|origin| !origin.trim().is_empty()) {
        builder = builder
            .header("access-control-allow-origin", origin)
            .header("access-control-allow-methods", method.unwrap_or("*"))
            .header(
                "access-control-allow-headers",
                requested.unwrap_or(PREFLIGHT_DEFAULT_HEADERS),
            )
            .header("access-control-max-age", PREFLIGHT_MAX_AGE);
    }
    builder
        .header("vary", PREFLIGHT_VARY)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::NO_CONTENT.into_response())
}

/// Header value as lossy UTF-8, or `None` when it is not presentable.
fn header_text<'a>(headers: &'a HeaderMap, name: &'a str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

// ---------------------------------------------------------------------------
// WebSocket tunnelling (A1)
// ---------------------------------------------------------------------------

/// Tunnels a negotiated inbound upgrade to the resolved upstream.
///
/// A real `tokio-tungstenite` client is opened first: if the upstream refuses
/// the handshake the caller still gets an RFC 9457 problem document instead of
/// a 101 that never completes. Only once the upstream answered 101 is the
/// inbound upgrade accepted, and two half-duplex pumps relay frames until
/// either side closes.
async fn tunnel(
    engine: &Engine,
    request: &ProxyRequest,
    upgrade: WebSocketUpgrade,
) -> Result<Response, DomainError> {
    let (target, extra) = engine.websocket_target_for(request).await?;
    let mut upgrade_request: UpgradeRequest<()> = target
        .clone()
        .into_client_request()
        .map_err(|_| upgrade_rejected(&request.alias))?;
    for (name, value) in extra {
        if let (Ok(name), Ok(value)) = (
            tokio_tungstenite::tungstenite::http::HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(&value),
        ) {
            upgrade_request.headers_mut().insert(name, value);
        }
    }

    let (upstream, _handshake) = tokio_tungstenite::connect_async(upgrade_request)
        .await
        .map_err(|_| upgrade_rejected(&request.alias))?;

    Ok(upgrade.on_upgrade(move |socket| pump(socket, upstream)))
}

/// The gateway-side failure of a WebSocket upgrade.
///
/// The detail deliberately names neither the upstream URL nor any header:
/// `ProtocolError` is rendered on the wire and must stay free of request data.
fn upgrade_rejected(alias: &str) -> DomainError {
    DomainError::ProtocolError {
        detail: "the WebSocket upgrade could not be completed".to_owned(),
        upstream_id: None,
        host: Some(alias.to_owned()),
    }
}

/// Pumps WebSocket frames between the caller and the upstream until either
/// side closes.
///
/// Both directions are driven by one `select`, so a half-closed connection is
/// drained rather than awaited forever; the first direction to finish drops
/// the other, which closes both sockets.
async fn pump(socket: WebSocket, upstream: UpstreamSocket) {
    let (mut caller_tx, mut caller_rx) = socket.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    let to_caller = Box::pin(forward(&mut upstream_rx, &mut caller_tx, to_axum));
    let to_upstream = Box::pin(forward(
        &mut caller_rx,
        &mut upstream_tx,
        |frame| Some(to_tungstenite(frame)),
    ));
    futures_util::future::select(to_caller, to_upstream).await;
}

/// Drives one direction of the frame pump until the source ends.
///
/// Review evidence (privilege boundary — data plane):
/// * Guardrail: DESIGN §4.3 "No PII" — frames are relayed as opaque bytes.
/// * Rationale: reading a frame only to drop it would be the same code, so the
///   conversion table is the single point where a frame could be inspected.
/// * Validation performed: the `proxy_tests` WebSocket test round-trips a
///   text and a binary frame through the registered router.
async fn forward<T, U, F, S, K, E>(source: &mut S, sink: &mut K, convert: F)
where
    S: futures_util::Stream<Item = Result<T, E>> + Unpin,
    K: futures_util::Sink<U> + Unpin,
    F: Fn(T) -> Option<U>,
{
    while let Some(frame) = source.next().await {
        let Ok(frame) = frame else {
            break;
        };
        let Some(converted) = convert(frame) else {
            continue;
        };
        if sink.send(converted).await.is_err() {
            break;
        }
    }
}

/// Converts an upstream frame into the axum shape.
fn to_axum(message: Message) -> Option<axum::extract::ws::Message> {
    match message {
        Message::Text(text) => Some(axum::extract::ws::Message::text(text.as_str())),
        Message::Binary(payload) => Some(axum::extract::ws::Message::Binary(payload)),
        Message::Ping(payload) => Some(axum::extract::ws::Message::Ping(payload)),
        Message::Pong(payload) => Some(axum::extract::ws::Message::Pong(payload)),
        Message::Close(frame) => Some(axum::extract::ws::Message::Close(
            frame.map(|frame| axum::extract::ws::CloseFrame {
                code: u16::from(frame.code),
                reason: axum::extract::ws::Utf8Bytes::from(frame.reason.as_str()),
            }),
        )),
        // Raw frames are not produced by a reading endpoint (tungstenite #268).
        Message::Frame(_) => None,
    }
}

/// Converts a caller frame into the tungstenite shape.
fn to_tungstenite(frame: axum::extract::ws::Message) -> Message {
    match frame {
        axum::extract::ws::Message::Text(text) => Message::text(text.as_str()),
        axum::extract::ws::Message::Binary(payload) => Message::Binary(payload),
        axum::extract::ws::Message::Ping(payload) => Message::Ping(payload),
        axum::extract::ws::Message::Pong(payload) => Message::Pong(payload),
        axum::extract::ws::Message::Close(close) => Message::Close(
            close.map(|close| {
                tokio_tungstenite::tungstenite::protocol::CloseFrame {
                    code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(
                        close.code,
                    ),
                    reason: tokio_tungstenite::tungstenite::Utf8Bytes::from(
                        close.reason.as_str(),
                    ),
                }
            }),
        ),
    }
}

// ---------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------

/// `GET /api/oagw/v1/metrics` — Prometheus exposition of the data plane.
pub async fn metrics(Extension(engine): Extension<Engine>) -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        engine.metrics().render(),
    )
        .into_response()
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
