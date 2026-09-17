// Created: 2026-09-04 by Constructor Tech
//! Streaming pass-through of the data plane
//! (`docs/PRD.md` §5.4, `cpt-cf-oagw-fr-streaming`).
//!
//! Two shapes of answer are forwarded instead of buffered:
//!
//! * a **server-sent-event** response, recognized by its
//!   `content-type: text/event-stream`, travels chunk by chunk with no
//!   timeout at all, so the caller observes every event as the upstream
//!   emits it;
//! * a **protocol upgrade** request — an `Upgrade` header named in
//!   `Connection` — is answered with the upstream's
//!   `101 Switching Protocols`, after which the connection is spliced byte
//!   for byte between the caller and the upstream, again without a timeout.
//!
//! The plugin chain still sees both: a streamed answer is described by its
//! head, which is where the response guards and transforms run
//! (`docs/ADR/0002-plugin-system.md` "Execution Order"), while the body is
//! never buffered and therefore never rewritten by a plugin. WebTransport is
//! an HTTP/3 flow (`docs/PRD.md` §5.4); without an HTTP/3 connector the gear
//! serves its `Upgrade` token like any other and splices it the same way.
//!
//! The WebSocket path needs no feature of its own: the inbound upgrade is
//! handed over by the server through `hyper::upgrade`, the upstream one is
//! negotiated with the same hyper-util client the plain proxy uses, and the
//! two halves are joined by [`tokio::io::copy_bidirectional`].

use axum::body::Body;
use http::header::{CONNECTION, UPGRADE};
use http::{HeaderMap, HeaderName, HeaderValue};
use hyper_util::rt::TokioIo;

use crate::dataplane::proxy::CancelProbe;
use crate::error::OagwError;

/// Media type announcing a server-sent-event stream (`docs/PRD.md` §5.4).
const EVENT_STREAM: &str = "text/event-stream";

/// `Connection` token of an upgrade request (RFC 9110 §7.6.1).
const UPGRADE_CONNECTION_TOKEN: &str = "upgrade";

/// `Upgrade` token of a WebSocket handshake (`docs/PRD.md` §5.4).
pub const WEBSOCKET_TOKEN: &str = "websocket";

/// Headers of an upgrade request the hop-by-hop strip removes and the upgrade
/// path restores: the tokens themselves plus the WebSocket handshake the
/// upstream needs to accept the session.
const UPGRADE_REQUEST_HEADERS: [HeaderName; 6] = [
    HeaderName::from_static("connection"),
    HeaderName::from_static("upgrade"),
    HeaderName::from_static("sec-websocket-key"),
    HeaderName::from_static("sec-websocket-version"),
    HeaderName::from_static("sec-websocket-protocol"),
    HeaderName::from_static("sec-websocket-extensions"),
];

/// Headers of an upgrade answer the caller has to see: the upstream's accept
/// proof and whatever subprotocol or extension it negotiated.
const UPGRADE_RESPONSE_HEADERS: [HeaderName; 3] = [
    HeaderName::from_static("sec-websocket-accept"),
    HeaderName::from_static("sec-websocket-protocol"),
    HeaderName::from_static("sec-websocket-extensions"),
];

/// Interval at which a spliced connection re-reads the gear shutdown.
///
/// A spliced connection has no timeout of its own, so the only way to bound
/// its task is to poll the cancellation probe: shutdown takes effect within
/// this interval and never waits for a peer to close.
const SHUTDOWN_GRANULARITY: std::time::Duration = std::time::Duration::from_millis(100);

// ------------------------------------------------------------------- SSE

/// `true` when the upstream response announces a server-sent-event stream.
///
/// Any non-`200` answer with the media type is streamed too: an upstream
/// answer is passed through untouched whatever its status
/// (`docs/ADR/0007-error-source-distinction.md`).
#[must_use]
pub fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with(EVENT_STREAM))
}

/// Frame timeout of a response body: `None` for a live stream, whose frames
/// are forwarded without any timeout.
#[must_use]
pub fn idle_timeout(
    headers: &HeaderMap,
    proxy_timeout: std::time::Duration,
) -> Option<std::time::Duration> {
    (!is_event_stream(headers)).then_some(proxy_timeout)
}

/// Forwards the body of an upstream response as it arrives.
///
/// With a frame timeout every frame is guarded on its own, so a stalled
/// upstream surfaces as `504 IdleTimeout` without imposing a total timeout on
/// the exchange; a server-sent-event stream is forwarded without one
/// (`cpt-cf-oagw-fr-streaming`).
#[must_use = "the forwarded body has to become the response body"]
pub fn response_body(
    incoming: hyper::body::Incoming,
    idle_timeout: Option<std::time::Duration>,
) -> Body {
    let stream = Body::new(incoming).into_data_stream();
    match idle_timeout {
        None => Body::from_stream(futures_util::TryStreamExt::map_err(stream, body_error)),
        Some(timeout) => Body::from_stream(idle_guarded(stream, timeout)),
    }
}

/// Maps a body read failure onto the documented stream error.
fn body_error(error: axum::Error) -> OagwError {
    OagwError::StreamAborted {
        detail: format!("the upstream response body was interrupted: {error}"),
    }
}

/// Guards every frame of `stream` with its own timeout, so a stalled upstream
/// surfaces as `504 IdleTimeout` instead of a hung response.
fn idle_guarded(
    stream: axum::body::BodyDataStream,
    timeout: std::time::Duration,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, OagwError>> {
    futures_util::stream::unfold(stream, move |mut stream| async move {
        match tokio::time::timeout(timeout, futures_util::StreamExt::next(&mut stream)).await {
            Ok(Some(Ok(bytes))) => Some((Ok(bytes), stream)),
            Ok(Some(Err(error))) => Some((Err(body_error(error)), stream)),
            Ok(None) => None,
            Err(_) => Some((
                Err(OagwError::IdleTimeout {
                    detail: format!("the upstream sent no data for {timeout:?}"),
                }),
                stream,
            )),
        }
    })
}

// ------------------------------------------------------------- upgrade path

/// `true` when the caller asks for a protocol upgrade
/// (`docs/PRD.md` §5.4): an `Upgrade` header named in `Connection`.
///
/// The check is header-only and runs before any body-level processing, so an
/// upgrade request is never buffered.
#[must_use]
pub fn is_upgrade_request(headers: &HeaderMap) -> bool {
    let requested = headers
        .get(UPGRADE)
        .is_some_and(|value| !value.as_bytes().is_empty());
    requested && connection_has_upgrade(headers)
}

/// `true` when the `Connection` header names the `upgrade` token.
fn connection_has_upgrade(headers: &HeaderMap) -> bool {
    crate::dataplane::headers::connection_tokens(headers)
        .iter()
        .any(|token| token == UPGRADE_CONNECTION_TOKEN)
}

/// Restores the upgrade headers of the caller on the outbound request.
///
/// [`crate::dataplane::headers::outbound_request_headers`] strips the
/// hop-by-hop headers of `docs/DESIGN.md` §3.2 — `Connection` and `Upgrade`
/// among them — because a plain proxied request must not carry them. An
/// upgrade request is the documented exception: the handshake has to reach
/// the upstream intact.
pub fn restore_upgrade_headers(inbound: &HeaderMap, outbound: &mut HeaderMap) {
    for name in &UPGRADE_REQUEST_HEADERS {
        let values: Vec<HeaderValue> = inbound.get_all(name).into_iter().cloned().collect();
        if values.is_empty() {
            continue;
        }
        outbound.remove(name);
        for value in values {
            outbound.append(name, value);
        }
    }
}

/// Headers of the `101 Switching Protocols` answer the caller receives: the
/// upgrade tokens plus the subprotocol and extensions the upstream negotiated.
#[must_use]
pub fn switching_protocols_headers(upstream: &HeaderMap, inbound: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
    let token = upstream
        .get(UPGRADE)
        .or_else(|| inbound.get(UPGRADE))
        .cloned();
    if let Some(token) = token {
        headers.insert(UPGRADE, token);
    }
    for name in &UPGRADE_RESPONSE_HEADERS {
        for value in upstream.get_all(name) {
            headers.append(name, value.clone());
        }
    }
    headers
}

/// Registers the upgrade of the upstream connection.
///
/// The future resolves once the upstream has handed the connection over; it
/// is awaited by [`splice`] so that the caller sees the `101` immediately.
#[must_use]
pub fn upstream_upgrade(
    response: &mut http::Response<hyper::body::Incoming>,
) -> hyper::upgrade::OnUpgrade {
    hyper::upgrade::on(response)
}

/// Inbound half of a protocol upgrade, captured by the proxy handler.
///
/// The server puts the upgrade handle into the request extensions when the
/// caller asks for one; capturing it here detaches the future from the parts
/// so the pipeline can hand it to the splicing task once the upstream has
/// agreed to the upgrade.
#[derive(Default)]
pub struct InboundUpgrade(Option<hyper::upgrade::OnUpgrade>);

impl InboundUpgrade {
    /// Captures the upgrade of the caller, when the connection supports one.
    #[must_use]
    pub fn capture(parts: &mut http::request::Parts) -> Self {
        Self(parts.extensions.remove::<hyper::upgrade::OnUpgrade>())
    }

    /// `true` when the caller asked for an upgrade the connection supports.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.0.is_some()
    }

    /// Detaches the upgrade, leaving the request without one.
    pub(crate) fn take(&mut self) -> Option<hyper::upgrade::OnUpgrade> {
        self.0.take()
    }
}

impl std::fmt::Debug for InboundUpgrade {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InboundUpgrade")
            .field("available", &self.0.is_some())
            .finish()
    }
}

/// Splices an established upgrade between the caller and the upstream.
///
/// Both halves are awaited here, so the `101` answer is written by the
/// handler while this task copies bytes in both directions. Either side
/// closing tears down both directions, and the gear cancellation token ends
/// the task within [`SHUTDOWN_GRANULARITY`] — an open session never delays a
/// shutdown.
pub(crate) async fn splice(
    on_client: hyper::upgrade::OnUpgrade,
    on_upstream: hyper::upgrade::OnUpgrade,
    cancelled: CancelProbe,
) {
    let (client, upstream) = tokio::join!(on_client, on_upstream);
    let (Ok(client), Ok(upstream)) = (client, upstream) else {
        tracing::debug!("the upgraded connection was never established on both sides");
        return;
    };
    let mut client = TokioIo::new(client);
    let mut upstream = TokioIo::new(upstream);
    let copy = tokio::io::copy_bidirectional(&mut client, &mut upstream);
    tokio::pin!(copy);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(SHUTDOWN_GRANULARITY) => {
                if cancelled() {
                    tracing::debug!("the gear is shutting down: the upgraded connection is torn down");
                    break;
                }
            }
            outcome = copy.as_mut() => {
                if let Err(error) = outcome {
                    tracing::debug!(%error, "the spliced connection failed");
                }
                break;
            }
        }
    }
    // Dropping both halves closes the connection in both directions, so the
    // caller and the upstream learn about the teardown at once.
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "streaming_tests.rs"]
mod streaming_tests;
