//! Streaming and Protocol Upgrades (`cpt-cf-oagw-feature-proxy-streaming`,
//! DECOMPOSITION entry 2.6): Server-Sent-Event response streaming and
//! WebSocket protocol-upgrade proxying, layered on top of entry 2.5's
//! resolved-request path (`crate::proxy::engine`).
//!
//! Both transports reuse proxy-core's alias/route resolution and header
//! rules for their initial request (`crate::proxy::engine`,
//! `crate::proxy::headers`) and are wired in by `crate::proxy::engine`'s
//! `forward_and_relay`/`forward_and_upgrade_websocket`. This module owns:
//! - SSE detection (`is_event_stream`) and the incremental relay that turns
//!   a [`crate::proxy::forward::StreamingUpstreamResponse`] into a
//!   committed, unbuffered [`Response`] (`cpt-cf-oagw-algo-stream-sse-detect-relay`).
//! - WebSocket upgrade recognition, the outbound handshake, and
//!   bidirectional frame relay (`cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`,
//!   `cpt-cf-oagw-algo-stream-websocket-frame-relay`).
//! - `StreamAborted` handling for both transports
//!   (`cpt-cf-oagw-algo-stream-abort-handling`).

use std::time::{Duration, Instant};

use axum::extract::FromRequestParts;
use axum::extract::ws::{
    CloseFrame as AxumCloseFrame, Message as AxumMessage, WebSocket, WebSocketUpgrade,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt as _;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message as TsMessage;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as TsCloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode as TsCloseCode;

use crate::audit::{AuditLogEntry, AuditLogFields};
use crate::error::{OagwError, OagwErrorKind};
use crate::model::upstream::Endpoint;
use crate::proxy::endpoint;
use crate::proxy::forward::{self, StreamingUpstreamResponse};
use crate::proxy::headers as hdr;
use crate::proxy::observe::ProxyMetrics;

/// Per-request context this module needs to record a `StreamAborted`
/// condition independently of -- and potentially long after -- the initial
/// `crate::proxy::observe::observe_completion` call that logged the
/// commit/handshake itself (`inst-stream-abort-handling-05`,
/// `inst-stream-abort-handling-10`).
#[derive(Debug, Clone)]
pub(crate) struct StreamAuditContext {
    pub request_id: String,
    pub tenant_id: Option<String>,
    pub principal_id: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub method: Option<String>,
    /// The inbound `/oagw/v1/proxy/...` instance path, for a rendered
    /// `OagwError`'s `instance` field.
    pub instance: String,
}

impl StreamAuditContext {
    /// Emit the `StreamAborted` audit-log entry and error metric
    /// (`cpt-cf-oagw-algo-stream-abort-handling`
    /// `inst-stream-abort-handling-05`/`inst-stream-abort-handling-10`).
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-05
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-10
    pub(crate) fn record_aborted(&self) {
        let fields = AuditLogFields {
            tenant_id: self.tenant_id.clone(),
            principal_id: self.principal_id.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            method: self.method.clone(),
            status: None,
            duration_ms: None,
        };
        AuditLogEntry::new("proxy_stream_aborted", "ERROR", &self.request_id, fields).emit();
        let host = self.host.as_deref().unwrap_or("unknown");
        let route = self.path.as_deref().unwrap_or("unknown");
        ProxyMetrics::global().record_error(host, route, "StreamAborted");
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-10
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-05

    /// Render the pre-commit/pre-handshake `StreamAborted` (`502`) gateway
    /// error (`inst-stream-abort-handling-02`).
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-02
    fn render_stream_aborted(&self, detail: impl Into<String>) -> Response {
        let mut error =
            OagwError::new(OagwErrorKind::StreamAborted, detail).with_instance(&self.instance);
        if let Some(host) = &self.host {
            error = error.with_host(host.clone());
        }
        if let Some(path) = &self.path {
            error = error.with_path(path.clone());
        }
        error = error.with_trace_id(self.request_id.clone());
        error.into_response()
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-02

    /// Render an ordinary (non-`StreamAborted`) gateway error for a
    /// WebSocket handshake failure that occurs before `101` reaches the
    /// client (`cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`
    /// step 5, `cpt-cf-oagw-algo-stream-abort-handling`
    /// `inst-stream-abort-handling-06`/`inst-stream-abort-handling-07`).
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-06
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-07
    fn render_handshake_error(&self, kind: OagwErrorKind, detail: impl Into<String>) -> Response {
        let mut error = OagwError::new(kind, detail).with_instance(&self.instance);
        if let Some(host) = &self.host {
            error = error.with_host(host.clone());
        }
        if let Some(path) = &self.path {
            error = error.with_path(path.clone());
        }
        error = error.with_trace_id(self.request_id.clone());
        error.into_response()
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-07
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-06
}

/// Outcome of committing (or failing to commit) an event-stream response
/// (`cpt-cf-oagw-state-stream-sse-session`).
pub(crate) enum SseOutcome {
    /// Status/headers committed and at least the first chunk is on its way
    /// to the client, or the stream ended cleanly with zero bytes -- either
    /// way, the response is final and the caller must not treat this as an
    /// error for metrics purposes.
    Streaming(Response),
    /// The upstream connection failed before any byte was committed to the
    /// client: the status line can still change, so this is rendered as the
    /// `StreamAborted` gateway error (`inst-stream-abort-handling-01`/`-02`).
    PreCommitAborted(Response),
}

/// Base media type of a `Content-Type` header, ignoring `charset` and any
/// other parameter (`cpt-cf-oagw-algo-stream-sse-detect-relay`
/// `inst-stream-sse-detect-relay-01`).
// @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-01
pub(crate) fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or("").trim())
        .is_some_and(|base| base.eq_ignore_ascii_case("text/event-stream"))
}
// @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-01

/// `cpt-cf-oagw-algo-stream-sse-detect-relay`: commit the classified
/// event-stream response and relay it to the client incrementally, without
/// ever buffering the complete body (`cpt-cf-oagw-principle-no-cache`).
///
/// The first chunk is awaited before the [`Response`] is constructed at
/// all: this is what distinguishes a pre-commit abort (nothing sent yet,
/// the status line can still change -- `cpt-cf-oagw-state-stream-sse-session`
/// `Opening` -> `Aborted`) from a post-commit abort (the response is
/// already on its way -- `Streaming` -> `Aborted`), per
/// `cpt-cf-oagw-algo-stream-abort-handling` steps 1-2.
// @cpt-algo:cpt-cf-oagw-algo-stream-sse-detect-relay:p2
// @cpt-dod:cpt-cf-oagw-dod-stream-sse-lifecycle:p1
// @cpt-dod:cpt-cf-oagw-dod-stream-abort-error:p1
// @cpt-state:cpt-cf-oagw-state-stream-sse-session:p2
// @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-03
// @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-04
// @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-05
// @cpt-begin:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-06
pub(crate) async fn relay_event_stream(
    upstream: StreamingUpstreamResponse,
    headers_config: Option<&crate::model::upstream::HeadersConfig>,
    audit_ctx: StreamAuditContext,
) -> SseOutcome {
    let mut response_headers = hdr::transform_response_headers(&upstream.headers, headers_config);
    // An incrementally relayed body has no fixed length known up front.
    response_headers.remove(header::CONTENT_LENGTH);
    response_headers.insert(
        axum::http::HeaderName::from_static(crate::error::ERROR_SOURCE_HEADER_NAME),
        axum::http::HeaderValue::from_static(crate::proxy::constants::ERROR_SOURCE_UPSTREAM),
    );

    let mut data_stream = upstream.response.into_limited_body().into_data_stream();

    // @cpt-begin:cpt-cf-oagw-state-stream-sse-session:p2:inst-stream-sse-session-state-01
    // @cpt-begin:cpt-cf-oagw-state-stream-sse-session:p2:inst-stream-sse-session-state-02
    let first_chunk = match data_stream.next().await {
        None => Bytes::new(),
        Some(Ok(chunk)) => chunk,
        Some(Err(_error)) => {
            // Opening -> Aborted: nothing has been committed to the client
            // yet, so the status line can still change
            // (`cpt-cf-oagw-algo-stream-abort-handling` step 1). Returning
            // the rendered `StreamAborted` error here is also that
            // algorithm's step 5 (`inst-stream-abort-handling-11`): the
            // applicable outcome for the branch just taken.
            // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-01
            // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-11
            return SseOutcome::PreCommitAborted(audit_ctx.render_stream_aborted(
                "the upstream connection failed before the event-stream response was committed to the client",
            ));
            // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-11
            // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-01
        }
    };
    // @cpt-end:cpt-cf-oagw-state-stream-sse-session:p2:inst-stream-sse-session-state-02
    // @cpt-end:cpt-cf-oagw-state-stream-sse-session:p2:inst-stream-sse-session-state-01

    // @cpt-begin:cpt-cf-oagw-state-stream-sse-session:p2:inst-stream-sse-session-state-03
    // @cpt-begin:cpt-cf-oagw-state-stream-sse-session:p2:inst-stream-sse-session-state-04
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-03
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-04
    let mut aborted_logged = false;
    let rest = data_stream.map(move |item| {
        if item.is_err() && !aborted_logged {
            // Streaming -> Aborted: the response is already committed, so
            // this can only end the connection, never a second response
            // (`inst-stream-abort-handling-03`/`-04`).
            aborted_logged = true;
            audit_ctx.record_aborted();
        }
        item
    });
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-04
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-03
    // @cpt-end:cpt-cf-oagw-state-stream-sse-session:p2:inst-stream-sse-session-state-04
    // @cpt-end:cpt-cf-oagw-state-stream-sse-session:p2:inst-stream-sse-session-state-03

    // `cpt-cf-oagw-flow-stream-sse-consumption` steps 4.3/4.3.1: the
    // returned `Response`'s body owns `data_stream`/`rest`, which in turn
    // owns the upstream connection's own body stream. If the client
    // disconnects while streaming, axum/hyper drops this `Response`'s body,
    // which transitively drops -- and so closes -- that upstream connection;
    // there is no separate branch to write because Rust's ownership chain
    // *is* the mechanism (`inst-stream-sse-consumption-07`/`-08`).
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-07
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-08
    let combined = futures_util::stream::once(async move { Ok(first_chunk) }).chain(rest);
    let mut response = Response::new(axum::body::Body::from_stream(combined));
    *response.status_mut() = upstream.status;
    *response.headers_mut() = response_headers;
    SseOutcome::Streaming(response)
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-08
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-07
}
// @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-06
// @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-05
// @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-04
// @cpt-end:cpt-cf-oagw-algo-stream-sse-detect-relay:p2:inst-stream-sse-detect-relay-03

/// Recognize an inbound request as a WebSocket upgrade attempt
/// (`cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`
/// `inst-stream-websocket-negotiation-01`): method `GET`, a `Connection`
/// header whose value contains the `Upgrade` token (case-insensitive), and
/// an `Upgrade` header value of `websocket`.
// @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-01
pub(crate) fn is_websocket_upgrade_request(method: &Method, headers: &HeaderMap) -> bool {
    if *method != Method::GET {
        return false;
    }
    let connection_has_upgrade = headers
        .get(header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
    let upgrade_is_websocket = headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    connection_has_upgrade && upgrade_is_websocket
}
// @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-01

/// Outcome of attempting a WebSocket upgrade
/// (`cpt-cf-oagw-state-stream-websocket-session`).
pub(crate) enum WebSocketOutcome {
    /// The upstream returned `101` and the client-side upgrade completed;
    /// frame relay is already spawned inside the returned response's
    /// `on_upgrade` continuation.
    Upgraded(Response),
    /// The handshake failed before completion: an ordinary gateway error,
    /// tagged with the error-metric bucket the caller should record.
    HandshakeFailed(Response, &'static str),
}

fn classify_ws_error(error: &WsError) -> (OagwErrorKind, &'static str) {
    match error {
        WsError::Io(_) => (OagwErrorKind::LinkUnavailable, "LinkUnavailable"),
        WsError::Http(_) | WsError::HttpFormat(_) => {
            (OagwErrorKind::ProtocolError, "ProtocolError")
        }
        _ => (OagwErrorKind::ProtocolError, "ProtocolError"),
    }
}

/// `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`: complete the
/// handshake with the resolved upstream endpoint, then hand off to
/// `relay_websocket_frames`. `outbound_headers` already carries the
/// preserved `Connection`/`Upgrade`/`Sec-WebSocket-*` headers
/// (`crate::proxy::headers::transform_request_headers` with
/// `preserve_upgrade = true`); `parts` still carries the client
/// connection's `hyper::upgrade::OnUpgrade` extension.
// @cpt-algo:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2
// @cpt-dod:cpt-cf-oagw-dod-stream-websocket-handshake:p1
// @cpt-dod:cpt-cf-oagw-dod-stream-plaintext-upstream:p1
// @cpt-state:cpt-cf-oagw-state-stream-websocket-session:p2
// @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-03
#[allow(clippy::too_many_arguments)]
/// Put the four per-connection RFC 6455 handshake headers onto the
/// outbound request. Returns `false` only if the generated key is somehow
/// not a legal header value.
///
/// This exists because `tungstenite::handshake::client::generate_request`
/// *validates* the request it is handed rather than synthesising a
/// handshake: it demands `Host`, `Connection`, `Upgrade`,
/// `Sec-WebSocket-Version` and `Sec-WebSocket-Key`, and reads the key back
/// out to verify the upstream's `Sec-WebSocket-Accept`. `Host` is
/// force-set from the resolved endpoint authority by
/// `transform_request_headers`; the rest must be added here.
fn apply_ws_handshake_headers(headers: &mut HeaderMap) -> bool {
    headers.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(
        header::SEC_WEBSOCKET_VERSION,
        HeaderValue::from_static("13"),
    );
    // A fresh key, not the client's: this is a separate handshake, and
    // `transform_request_headers` intentionally drops the inbound one.
    match HeaderValue::try_from(generate_key()) {
        Ok(key) => {
            headers.insert(header::SEC_WEBSOCKET_KEY, key);
            true
        }
        Err(_error) => false,
    }
}

pub(crate) async fn forward_and_upgrade_websocket(
    mut parts: Parts,
    outbound_headers: HeaderMap,
    endpoint: Endpoint,
    outbound_path: &str,
    outbound_query: &[(String, String)],
    timeout_secs: u32,
    audit_ctx: StreamAuditContext,
) -> WebSocketOutcome {
    let ws_upgrade: WebSocketUpgrade =
        match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => upgrade,
            Err(rejection) => {
                return WebSocketOutcome::HandshakeFailed(
                    audit_ctx.render_handshake_error(
                        OagwErrorKind::ValidationError,
                        format!("not a valid WebSocket upgrade request: {rejection}"),
                    ),
                    "ValidationError",
                );
            }
        };

    let scheme = if endpoint::is_plaintext_scheme(endpoint.scheme) {
        "ws"
    } else {
        "wss"
    };
    let url = forward::compose_url(
        scheme,
        &endpoint.host,
        endpoint.port,
        outbound_path,
        outbound_query,
    );
    let Ok(uri) = url.parse::<axum::http::Uri>() else {
        return WebSocketOutcome::HandshakeFailed(
            audit_ctx.render_handshake_error(
                OagwErrorKind::ProtocolError,
                "failed to build the outbound WebSocket handshake URL",
            ),
            "ProtocolError",
        );
    };
    let mut request = match axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri(uri)
        .body(())
    {
        Ok(request) => request,
        Err(_error) => {
            return WebSocketOutcome::HandshakeFailed(
                audit_ctx.render_handshake_error(
                    OagwErrorKind::ProtocolError,
                    "failed to build the outbound WebSocket handshake request",
                ),
                "ProtocolError",
            );
        }
    };
    *request.headers_mut() = outbound_headers;
    // `tungstenite::handshake::client::generate_request` does NOT synthesise
    // a handshake: it requires `Host`, `Connection`, `Upgrade`,
    // `Sec-WebSocket-Version` and `Sec-WebSocket-Key` to already be present
    // on the request it is handed, and it reads the key back out to verify
    // the upstream's `Sec-WebSocket-Accept`. `transform_request_headers`
    // deliberately drops the client's `Sec-WebSocket-*` headers (they are
    // per-connection, and this hop is a *separate* handshake), so without
    // the four inserts below `connect_async` rejects our own request with
    // "Missing, duplicated or incorrect header sec-websocket-key" before it
    // ever dials the upstream -- i.e. every upgrade would 502.
    // `Host` is force-set from the resolved endpoint authority by
    // `transform_request_headers`, so it is already correct here.
    if !apply_ws_handshake_headers(request.headers_mut()) {
        return WebSocketOutcome::HandshakeFailed(
            audit_ctx.render_handshake_error(
                OagwErrorKind::ProtocolError,
                "failed to generate the outbound WebSocket handshake key",
            ),
            "ProtocolError",
        );
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-03

    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-04
    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-07
    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-08
    // @cpt-begin:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-02
    // @cpt-dod:cpt-cf-oagw-dod-stream-timeout-exemption:p1
    // The `proxy_timeout_secs` deadline bounds only this pre-handshake
    // phase (`cpt-cf-oagw-dod-stream-timeout-exemption`); once
    // `connect_async` returns successfully below, the deadline is never
    // consulted again for the life of the session.
    let deadline = Instant::now() + Duration::from_secs(u64::from(timeout_secs));
    let remaining = deadline.saturating_duration_since(Instant::now());
    let handshake =
        tokio::time::timeout(remaining, tokio_tungstenite::connect_async(request)).await;
    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-09
    let (upstream_ws, _handshake_response) = match handshake {
        Err(_elapsed) => {
            return WebSocketOutcome::HandshakeFailed(
                audit_ctx.render_handshake_error(
                    OagwErrorKind::ConnectionTimeout,
                    "the proxy_timeout_secs deadline expired before the WebSocket handshake completed",
                ),
                "ConnectionTimeout",
            );
        }
        Ok(Err(error)) => {
            let (kind, metric) = classify_ws_error(&error);
            return WebSocketOutcome::HandshakeFailed(
                audit_ctx.render_handshake_error(
                    kind,
                    format!("the upstream refused or failed to complete the WebSocket handshake: {error}"),
                ),
                metric,
            );
        }
        Ok(Ok(pair)) => pair,
    };
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-09
    // @cpt-end:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-02
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-08
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-07
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-04

    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-05
    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-06
    // @cpt-begin:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-01
    // `WebSocketUpgrade::on_upgrade` computes its own `Sec-WebSocket-Accept`
    // from the client's original `Sec-WebSocket-Key` and completes the
    // upgrade on the client side; the upstream's own `101` (already
    // verified by `connect_async` above) is what authorizes relaying frames
    // rather than being copied onto the client response byte-for-byte.
    let response = ws_upgrade.on_upgrade(move |client_socket| async move {
        relay_websocket_frames(client_socket, upstream_ws, audit_ctx).await;
    });
    WebSocketOutcome::Upgraded(response)
    // @cpt-end:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-01
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-06
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-05
}

fn axum_to_tungstenite(message: AxumMessage) -> TsMessage {
    match message {
        AxumMessage::Text(text) => TsMessage::text(text.as_str()),
        AxumMessage::Binary(data) => TsMessage::Binary(data),
        AxumMessage::Ping(data) => TsMessage::Ping(data),
        AxumMessage::Pong(data) => TsMessage::Pong(data),
        AxumMessage::Close(Some(frame)) => TsMessage::Close(Some(TsCloseFrame {
            code: TsCloseCode::from(frame.code),
            reason: frame.reason.as_str().into(),
        })),
        AxumMessage::Close(None) => TsMessage::Close(None),
    }
}

fn tungstenite_to_axum(message: TsMessage) -> Option<AxumMessage> {
    match message {
        TsMessage::Text(text) => Some(AxumMessage::Text(text.as_str().into())),
        TsMessage::Binary(data) => Some(AxumMessage::Binary(data)),
        TsMessage::Ping(data) => Some(AxumMessage::Ping(data)),
        TsMessage::Pong(data) => Some(AxumMessage::Pong(data)),
        TsMessage::Close(Some(frame)) => Some(AxumMessage::Close(Some(AxumCloseFrame {
            code: u16::from(frame.code),
            reason: frame.reason.as_str().into(),
        }))),
        TsMessage::Close(None) => Some(AxumMessage::Close(None)),
        // Raw `Frame` messages are a tungstenite-internal detail that is
        // never surfaced to a `WebSocketStream` caller in practice, mirrors
        // axum's own `Message::from_tungstenite` handling of the variant.
        TsMessage::Frame(_) => None,
    }
}

/// `cpt-cf-oagw-algo-stream-websocket-frame-relay`: relay frames verbatim
/// in both directions until either side sends a close frame (propagated,
/// with its close code, to the other side) or a connection terminates
/// abnormally without one (`cpt-cf-oagw-algo-stream-abort-handling`'s
/// connection-level-closure branch).
// @cpt-algo:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2
// @cpt-dod:cpt-cf-oagw-dod-stream-websocket-frame-relay:p1
// @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-01
// @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-02
// @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-03
// @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-04
async fn relay_websocket_frames(
    mut client: WebSocket,
    mut upstream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    audit_ctx: StreamAuditContext,
) {
    // @cpt-begin:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-03
    // @cpt-begin:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-04
    // @cpt-begin:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-05
    let mut closed_with_close_frame = false;
    loop {
        tokio::select! {
            client_msg = client.recv() => {
                match client_msg {
                    Some(Ok(msg)) => {
                        // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-05
                        // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-06
                        let is_close = matches!(msg, AxumMessage::Close(_));
                        if upstream.send(axum_to_tungstenite(msg)).await.is_err() {
                            break;
                        }
                        if is_close {
                            closed_with_close_frame = true;
                            break;
                        }
                        // @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-06
                        // @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-05
                    }
                    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-08
                    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-09
                    Some(Err(_)) | None => break,
                    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-09
                    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-08
                }
            }
            upstream_msg = upstream.next() => {
                match upstream_msg {
                    Some(Ok(msg)) => {
                        let is_close = matches!(msg, TsMessage::Close(_));
                        if let Some(converted) = tungstenite_to_axum(msg)
                            && client.send(converted).await.is_err()
                        {
                            break;
                        }
                        if is_close {
                            closed_with_close_frame = true;
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-05
    // @cpt-end:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-04
    // @cpt-end:cpt-cf-oagw-state-stream-websocket-session:p2:inst-stream-websocket-session-state-03

    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-07
    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-10
    // Dropping both `client`/`upstream` here closes both underlying
    // connections; if the loop ended without either side ever sending a
    // close frame, this was an abnormal termination
    // (`cpt-cf-oagw-algo-stream-abort-handling` steps 4).
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-08
    // @cpt-begin:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-09
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-14
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-15
    if !closed_with_close_frame {
        audit_ctx.record_aborted();
    }
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-15
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-14
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-09
    // @cpt-end:cpt-cf-oagw-algo-stream-abort-handling:p2:inst-stream-abort-handling-08
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-10
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-07
}
// @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-04
// @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-03
// @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-02
// @cpt-end:cpt-cf-oagw-algo-stream-websocket-frame-relay:p2:inst-stream-websocket-frame-relay-01

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// Regression test for a defect found by driving the running release
    /// server: every WebSocket upgrade answered `502 Bad Gateway` with
    /// "Missing, duplicated or incorrect header sec-websocket-key", in
    /// 0 ms and without ever dialing the upstream.
    ///
    /// The cause was that the outbound request's headers are replaced
    /// wholesale by `transform_request_headers`' output, which correctly
    /// drops the client's per-connection `Sec-WebSocket-*` headers -- and
    /// `tungstenite`'s `generate_request` validates rather than synthesises
    /// the handshake, so it rejected our own request.
    ///
    /// This asserts the real predicate that was violated: `tungstenite`
    /// accepts the request we hand `connect_async`. Every pre-existing
    /// WebSocket test passed while the live server was broken, because none
    /// of them checked this.
    #[test]
    fn outbound_handshake_request_is_accepted_by_tungstenites_own_validator() {
        use tokio_tungstenite::tungstenite::handshake::client::generate_request;

        // Exactly what `transform_request_headers(.., preserve_upgrade = true)`
        // yields for a WebSocket upgrade: `Host` force-set from the resolved
        // endpoint, `Connection`/`Upgrade` reinstated, and NO `Sec-WebSocket-*`.
        let transformed = headers(&[
            ("host", "upstream.internal:9100"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
        ]);

        // Negative control: without the handshake headers, tungstenite
        // rejects the request -- this is the live 502 reproduced in-process.
        let mut without = axum::http::Request::builder()
            .method(Method::GET)
            .uri("ws://upstream.internal:9100/")
            .body(())
            .unwrap();
        *without.headers_mut() = transformed.clone();
        assert!(
            generate_request(without).is_err(),
            "precondition: tungstenite must reject a handshake with no Sec-WebSocket-Key"
        );

        // With the fix, tungstenite accepts it and echoes back the key it
        // will later use to verify `Sec-WebSocket-Accept`.
        let mut with = axum::http::Request::builder()
            .method(Method::GET)
            .uri("ws://upstream.internal:9100/")
            .body(())
            .unwrap();
        *with.headers_mut() = transformed;
        assert!(apply_ws_handshake_headers(with.headers_mut()));
        for required in [
            "host",
            "connection",
            "upgrade",
            "sec-websocket-version",
            "sec-websocket-key",
        ] {
            assert!(
                with.headers().contains_key(required),
                "tungstenite requires the `{required}` header"
            );
        }
        let (wire, key) = generate_request(with).expect("tungstenite must accept the request");
        assert!(
            !key.is_empty(),
            "a key must be returned for accept-verification"
        );
        assert!(String::from_utf8_lossy(&wire).contains("Sec-WebSocket-Key"));
    }

    /// Two consecutive handshakes must not reuse a key: the key is
    /// per-connection and is what verifies each upstream's
    /// `Sec-WebSocket-Accept`.
    #[test]
    fn each_handshake_gets_a_fresh_key() {
        let mut first = HeaderMap::new();
        let mut second = HeaderMap::new();
        assert!(apply_ws_handshake_headers(&mut first));
        assert!(apply_ws_handshake_headers(&mut second));
        assert_ne!(
            first.get(header::SEC_WEBSOCKET_KEY),
            second.get(header::SEC_WEBSOCKET_KEY)
        );
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::try_from(*name).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn event_stream_content_type_is_detected_ignoring_charset() {
        let h = headers(&[("content-type", "text/event-stream; charset=utf-8")]);
        assert!(is_event_stream(&h));
    }

    #[test]
    fn non_event_stream_content_type_is_not_detected() {
        let h = headers(&[("content-type", "application/json")]);
        assert!(!is_event_stream(&h));
    }

    #[test]
    fn missing_content_type_is_not_detected() {
        assert!(!is_event_stream(&HeaderMap::new()));
    }

    #[test]
    fn get_with_connection_upgrade_and_websocket_is_recognized() {
        let h = headers(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
        assert!(is_websocket_upgrade_request(&Method::GET, &h));
    }

    #[test]
    fn connection_header_with_multiple_tokens_is_still_recognized() {
        let h = headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "websocket"),
        ]);
        assert!(is_websocket_upgrade_request(&Method::GET, &h));
    }

    #[test]
    fn post_method_is_never_a_websocket_upgrade() {
        let h = headers(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
        assert!(!is_websocket_upgrade_request(&Method::POST, &h));
    }

    #[test]
    fn missing_upgrade_header_is_not_recognized() {
        let h = headers(&[("connection", "Upgrade")]);
        assert!(!is_websocket_upgrade_request(&Method::GET, &h));
    }

    #[test]
    fn non_websocket_upgrade_value_is_not_recognized() {
        let h = headers(&[("connection", "Upgrade"), ("upgrade", "h2c")]);
        assert!(!is_websocket_upgrade_request(&Method::GET, &h));
    }

    fn audit_ctx() -> StreamAuditContext {
        StreamAuditContext {
            request_id: "req-1".to_owned(),
            tenant_id: Some("tenant-1".to_owned()),
            principal_id: Some("principal-1".to_owned()),
            host: Some("svc.example.com".to_owned()),
            path: Some("/v1/stream".to_owned()),
            method: Some("GET".to_owned()),
            instance: "/oagw/v1/proxy/svc".to_owned(),
        }
    }

    #[test]
    fn render_stream_aborted_is_a_502_with_gateway_error_source() {
        let response = audit_ctx().render_stream_aborted("upstream failed before commit");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response
                .headers()
                .get(crate::error::ERROR_SOURCE_HEADER_NAME)
                .and_then(|v| v.to_str().ok()),
            Some(crate::error::ERROR_SOURCE_GATEWAY)
        );
    }

    #[test]
    fn record_aborted_increments_the_stream_aborted_error_metric() {
        let ctx = audit_ctx();
        let before = ProxyMetrics::global().errors_total(
            ctx.host.as_deref().unwrap(),
            ctx.path.as_deref().unwrap(),
            "StreamAborted",
        );
        ctx.record_aborted();
        let after = ProxyMetrics::global().errors_total(
            ctx.host.as_deref().unwrap(),
            ctx.path.as_deref().unwrap(),
            "StreamAborted",
        );
        assert_eq!(after, before + 1);
    }

    #[test]
    fn message_conversion_round_trips_text_binary_and_close() {
        let text = AxumMessage::Text("hello".into());
        let ts = axum_to_tungstenite(text.clone());
        assert_eq!(ts, TsMessage::text("hello"));
        assert_eq!(tungstenite_to_axum(ts), Some(text));

        let binary = AxumMessage::Binary(Bytes::from_static(b"\x01\x02"));
        let ts = axum_to_tungstenite(binary.clone());
        assert_eq!(tungstenite_to_axum(ts), Some(binary));

        let close = AxumMessage::Close(Some(AxumCloseFrame {
            code: 4000,
            reason: "bye".into(),
        }));
        let ts = axum_to_tungstenite(close.clone());
        assert_eq!(tungstenite_to_axum(ts), Some(close));
    }
}
