//! Byte-streamed forwarding: plain HTTP bodies and Server-Sent Events.
//!
//! [`forward_streaming`] is the streaming leg of the data plane. The request
//! body is handed to the upstream while it is still arriving, the upstream
//! response body is relayed frame by frame, and a `content-type:
//! text/event-stream` response is passed through event by event without
//! re-framing — no re-serializing, merging, splitting or re-ordering of events
//! (R1).
//!
//! ```text
//! validate the request framing (transfer-encoding, content-length)
//!   -> dial the upstream over one fresh HTTP/1.1 connection
//!   -> peek the first body frame
//!        -> failure           -> 502 ...downstream.error.v1 (no head sent yet)
//!        -> clean end of body -> the head is sent and the body ends with it
//!        -> a frame           -> the head is sent with X-OAGW-Error-Source:
//!                                upstream, then frames are relayed as they come
//! ```
//!
//! The peek is a single frame, not the stream: it is what lets the gear tell an
//! upstream that failed before it produced anything from one that failed while
//! relaying (R8), and the frame is re-emitted first, so the byte stream the
//! caller receives is exactly the byte stream the upstream produced.
//!
//! The lifecycle of the SSE use case is handled by [`RelayStream`]: when the
//! upstream closes, the relay ends and the client response ends with it; when
//! the client disconnects, axum drops the response body, the relay is dropped
//! and the upstream body — and with it the upstream connection — is closed, so
//! an upstream is never left streaming to nobody (R2).
//!
//! This leg dials plaintext upstreams, which is what makes both bodies
//! streamable: the connection belongs to the call and is dropped with it, so a
//! streamed response is never parked in a pool. A TLS upstream keeps Phase 4's
//! leg, whose response body already streams from the shared client and whose
//! client only forwards a whole request body.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, BodyDataStream};
use axum::extract::Request;
use axum::http::{HeaderMap, Method, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::domain::model::{Endpoint, Upstream};
use crate::error::{ErrorSource, GatewayError, GatewayErrorKind};
use crate::proxy::forward::{MAX_REQUEST_BODY_BYTES, ProxyService};
use crate::proxy::headers as gateway_headers;
use crate::proxy::matcher::http_method;
use crate::proxy::resolver::ResolvedUpstream;

/// The hard request-body limit of DESIGN.md §3.2 "Body Validation Rules" (R11),
/// in whole mebibytes, for the messages of the 413 problem.
const MAX_REQUEST_BODY_MB: usize = 100;

/// The client half of a freshly dialled streamed upstream connection.
type PlainSender = hyper::client::conn::http1::SendRequest<axum::body::Body>;

/// An upstream response whose head has arrived and whose body is still
/// streaming.
struct StreamedUpstream {
    /// The status and headers of the upstream response, as received.
    head: axum::http::response::Parts,
    /// The body of the upstream response, streaming.
    body: BodyDataStream,
    /// The URL the response was dialled at, for logs and problem details.
    url: String,
}

/// How the request body is framed on the upstream leg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFraming {
    /// A declared `Content-Length` of the given size, forwarded as declared.
    Sized(usize),
    /// `Transfer-Encoding: chunked` (or no declared length): framed by the
    /// caller on the upstream leg, or body-less there when the method cannot
    /// carry a body at all.
    Chunked,
}

/// Forwards a request and its response as byte streams (R1-R3, R6, R8).
///
/// # Errors
///
/// Every failure is returned as a [`GatewayError`], which renders as an RFC
/// 9457 problem document. Errors carry the host of the endpoint the call was
/// dialled at.
pub async fn forward_streaming(
    service: &ProxyService,
    resolved: &ResolvedUpstream,
    route: crate::proxy::RouteMatch,
    method: Method,
    headers: HeaderMap,
    request: Request,
    endpoint: &Endpoint,
) -> Response {
    if http_method(&method).is_none() {
        return GatewayError::new(
            GatewayErrorKind::RouteNotFound,
            format!(
                "no route of this upstream matches `{method} {}`",
                route.upstream_path
            ),
        )
        .into_response();
    }

    let framing = match validate_request_framing(&headers) {
        Ok(framing) => framing,
        Err(error) => return error.into_response(),
    };

    let request_headers = gateway_headers::build_request_headers(
        &headers,
        &resolved.upstream,
        endpoint,
        declared_length(framing),
    );
    let mut request_headers = request_headers;

    // A body the caller framed itself is not declared here either: the
    // upstream leg frames it chunked, or sends none at all.
    if framing == BodyFraming::Chunked {
        request_headers.remove(header::CONTENT_LENGTH);
    }

    let url = target_url(endpoint, &route.upstream_path, route.query.as_deref());

    match dial_plaintext(
        endpoint,
        &method,
        &url,
        request_headers,
        framing,
        request.into_body(),
        service.config.config().proxy_timeout(),
    )
    .await
    {
        Ok(upstream) => stream_upstream(&resolved.upstream, endpoint, upstream).await,
        Err(mut error) => {
            error = error.with_host(endpoint.host.as_str());
            error.into_response()
        }
    }
}

/// The `Content-Length` a streamed request body is declared with.
fn declared_length(framing: BodyFraming) -> usize {
    match framing {
        BodyFraming::Sized(declared) => declared,
        BodyFraming::Chunked => 0,
    }
}

/// Validates the framing a streamed request body is forwarded under (R10-R12).
///
/// The declared `Content-Length` is checked before anything is dialled, so an
/// oversized or non-integer length never reaches the upstream. Unlike the
/// buffered leg, the body is not compared against its declaration after the
/// fact: the stream is counted while it is relayed and aborted as soon as it
/// leaves the frame it declared.
///
/// # Errors
///
/// Returns 400 for a `Transfer-Encoding` other than `chunked` and for a
/// non-integer `Content-Length`, and 413 for a declared length beyond the hard
/// limit.
fn validate_request_framing(headers: &HeaderMap) -> Result<BodyFraming, GatewayError> {
    for value in headers.get_all(header::TRANSFER_ENCODING) {
        let value = value.to_str().unwrap_or_default();

        if value.trim().eq_ignore_ascii_case("chunked") {
            continue;
        }

        return Err(GatewayError::validation(
            format!(
                "`transfer-encoding: {value}` is not supported; only `chunked` request bodies \
                 are streamed"
            ),
            "transfer-encoding",
        ));
    }

    let Some(value) = headers.get(header::CONTENT_LENGTH) else {
        return Ok(BodyFraming::Chunked);
    };

    let text = value.to_str().unwrap_or_default().trim();
    let declared = text.parse::<usize>().map_err(|_| {
        GatewayError::validation(
            format!("`content-length: {text}` is not a valid byte count"),
            "content-length",
        )
    })?;

    if declared > MAX_REQUEST_BODY_BYTES {
        return Err(payload_too_large(declared));
    }

    Ok(BodyFraming::Sized(declared))
}

/// 413 `cf.oagw.payload.too_large.v1` (R11).
fn payload_too_large(bytes: usize) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::PayloadTooLarge,
        format!("the request body exceeds the {MAX_REQUEST_BODY_MB} MB hard limit ({bytes} bytes)"),
    )
}

/// Dials a plaintext upstream and streams both bodies (R2, R3, R13, R15).
///
/// # Errors
///
/// Returns 400 for a body that leaves its declared frame, 413 for a body beyond
/// the hard limit, 504 when the dial does not complete within the proxy
/// timeout, 502 when the connection or the HTTP exchange fails, and 500 when
/// the outbound request cannot be assembled at all.
async fn dial_plaintext(
    endpoint: &Endpoint,
    method: &Method,
    url: &str,
    headers: HeaderMap,
    framing: BodyFraming,
    body: Body,
    timeout: Duration,
) -> Result<StreamedUpstream, GatewayError> {
    let stream = connect(endpoint, url, timeout).await?;
    let (mut sender, connection) = tokio::time::timeout(
        timeout,
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await
    .map_err(|_| upstream_timeout(url))?
    .map_err(|error| unreachable_upstream(url, error))?;

    // The connection is driven on its own task and dropped with the call.
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            debug!(error = %error, "the streamed upstream connection closed");
        }
    });

    let (outbound, guard) = build_outbound(method, url, headers, framing, body)?;
    let head = tokio::time::timeout(timeout, send(&mut sender, outbound, &guard))
        .await
        .map_err(|_| upstream_timeout(url))?
        .map_err(|failure| {
            // A body the guard rejected is reported as that body's problem
            // rather than as a transport failure.
            failure.problem(url)
        })?;

    let (head, body) = head.into_parts();

    Ok(StreamedUpstream {
        head,
        body: Body::new(body).into_data_stream(),
        url: url.to_owned(),
    })
}

/// Connects to a plaintext endpoint (R13, R15).
///
/// # Errors
///
/// Returns 504 when the connect does not complete within the proxy timeout and
/// 502 when the endpoint cannot be reached.
async fn connect(
    endpoint: &Endpoint,
    url: &str,
    timeout: Duration,
) -> Result<TcpStream, GatewayError> {
    let address = format!("{}:{}", endpoint.host.as_str(), endpoint.port);

    tokio::time::timeout(timeout, TcpStream::connect(&address))
        .await
        .map_err(|_| upstream_timeout(url))?
        .map_err(|error| unreachable_upstream(url, error))
}

/// The failure of a streamed upstream exchange (R15).
struct DialFailure {
    /// The problem the streamed request body recorded, when it did.
    guard: Option<GatewayError>,
    /// The transport error, when the body did not produce a problem.
    transport: hyper::Error,
}

impl DialFailure {
    /// The problem the failure is reported as (R8, R15).
    fn problem(self, url: &str) -> GatewayError {
        self.guard
            .unwrap_or_else(|| map_stream_error(self.transport, url))
    }
}

/// Sends a streamed request and awaits the upstream response head (R15).
///
/// # Errors
///
/// Returns the failure of the exchange, carrying the problem the request body
/// recorded when it produced one.
async fn send(
    sender: &mut PlainSender,
    request: hyper::Request<axum::body::Body>,
    guard: &BodyGuard,
) -> Result<hyper::Response<hyper::body::Incoming>, DialFailure> {
    let failure = |error: hyper::Error| DialFailure {
        guard: guard.take(),
        transport: error,
    };

    let () = std::future::poll_fn(|cx| sender.poll_ready(cx))
        .await
        .map_err(failure)?;

    sender.send_request(request).await.map_err(failure)
}

/// Assembles the outbound request of a streamed call (R3, R7).
///
/// # Errors
///
/// Returns 500 when the HTTP types refuse the request that was assembled.
fn build_outbound(
    method: &Method,
    url: &str,
    headers: HeaderMap,
    framing: BodyFraming,
    body: Body,
) -> Result<(hyper::Request<axum::body::Body>, BodyGuard), GatewayError> {
    let uri: hyper::http::Uri = url.parse().map_err(|error| {
        GatewayError::new(
            GatewayErrorKind::Internal,
            format!("the upstream URL `{url}` is not a usable URI: {error}"),
        )
    })?;

    let counted = RequestBodyStream {
        body: body.into_data_stream(),
        framing,
        guard: BodyGuard::default(),
        total: 0,
    };
    let guard = counted.guard.clone();

    // The upstream is an origin server, so the request target it is sent is
    // origin-form (RFC 9112 §3.2.2): a raw HTTP/1 connection writes the URI it
    // is given verbatim, and an absolute URI would leave the gateway forwarding
    // `GET http://host:80/path` to an upstream that serves `/path`. The
    // authority travels in the rewritten `Host` header instead, and the
    // absolute URL stays in `url` for the logs and the problem details.
    let target = uri
        .path_and_query()
        .map_or_else(|| "/".to_owned(), |target| target.as_str().to_owned());

    let mut builder = hyper::Request::builder().method(method.clone()).uri(target);

    if let Some(target) = builder.headers_mut() {
        *target = headers;
    }

    let request = builder.body(Body::from_stream(counted)).map_err(|error| {
        GatewayError::new(
            GatewayErrorKind::Internal,
            format!("the streamed upstream request could not be built: {error}"),
        )
    })?;

    Ok((request, guard))
}

/// Turns a dialled upstream response into the streamed response the caller
/// receives (R1, R2, R6, R8).
async fn stream_upstream(
    upstream: &Upstream,
    endpoint: &Endpoint,
    upstream_response: StreamedUpstream,
) -> Response {
    let StreamedUpstream {
        mut head,
        body,
        url,
    } = upstream_response;
    let alias = upstream.alias().to_owned();
    let streamed = is_sse(&head.headers);

    gateway_headers::build_response_headers(upstream, &mut head.headers);

    let mut body = body;
    let first = body.next().await;

    let relay = match first {
        Some(Ok(first)) => {
            if streamed {
                info!(upstream = %alias, url = %url, "the SSE stream is open");
            }

            RelayStream {
                upstream: body,
                buffered: Some(first),
                call: StreamedCall { alias, url },
                closed: false,
            }
        }
        // The upstream closed its body without sending anything: the client
        // response ends with it, which is a close rather than a failure.
        None => RelayStream {
            upstream: body,
            buffered: None,
            call: StreamedCall { alias, url },
            closed: true,
        },
        // The upstream failed before a single byte was relayed: the response
        // head has not been sent, so the caller still gets a problem document.
        Some(Err(error)) => {
            return GatewayError::new(
                GatewayErrorKind::DownstreamError,
                format!(
                    "the upstream at `{url}` failed before its body could be streamed: {error}"
                ),
            )
            .with_host(endpoint.host.as_str())
            .into_response();
        }
    };

    let mut response = Response::from_parts(head, Body::from_stream(relay));

    ErrorSource::Upstream.set_on(&mut response);

    response
}

/// Whether an upstream response is a Server-Sent Events stream (R1).
fn is_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .any(|part| part.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// The absolute URL of a streamed call, for logs and problem details.
fn target_url(endpoint: &Endpoint, path: &str, query: Option<&str>) -> String {
    let scheme = if endpoint.is_plaintext() {
        "http"
    } else {
        "https"
    };
    let authority = format!("{}:{}", endpoint.host.as_str(), endpoint.port);

    match query {
        Some(query) => format!("{scheme}://{authority}{path}?{query}"),
        None => format!("{scheme}://{authority}{path}"),
    }
}

/// The upstream call a streamed body belongs to, for its lifecycle logs.
struct StreamedCall {
    /// The alias of the upstream the stream came from.
    alias: String,
    /// The URL the stream is relayed from.
    url: String,
}

/// The upstream body of a streamed response, relayed frame by frame.
///
/// Dropping the relay — which is what a client disconnect does to a response
/// body — closes the upstream body and with it the upstream connection (R2).
struct RelayStream {
    /// The body of the upstream response, still streaming.
    upstream: BodyDataStream,
    /// The frame peeked before the response head was sent.
    buffered: Option<Bytes>,
    /// The call the stream belongs to.
    call: StreamedCall,
    /// Whether the stream reached a defined end (a close or a failure).
    closed: bool,
}

impl Stream for RelayStream {
    type Item = Result<Bytes, GatewayError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        if let Some(frame) = this.buffered.take() {
            return Poll::Ready(Some(Ok(frame)));
        }

        match Pin::new(&mut this.upstream).poll_next(cx) {
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(error))) => {
                this.closed = true;
                warn!(
                    upstream = %this.call.alias,
                    url = %this.call.url,
                    source = "gateway",
                    error = %error,
                    "the upstream failed while its response was being streamed"
                );

                Poll::Ready(Some(Err(this.mid_stream_failure())))
            }
            Poll::Ready(None) => {
                this.closed = true;
                debug!(
                    upstream = %this.call.alias,
                    url = %this.call.url,
                    "the upstream closed its stream; closing the client response"
                );

                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl RelayStream {
    /// The problem a failure while relaying is reported as (R8).
    fn mid_stream_failure(&self) -> GatewayError {
        GatewayError::new(
            GatewayErrorKind::DownstreamError,
            format!(
                "the upstream at `{}` failed while its response was being streamed",
                self.call.url
            ),
        )
    }
}

impl Drop for RelayStream {
    fn drop(&mut self) {
        if !self.closed {
            warn!(
                upstream = %self.call.alias,
                url = %self.call.url,
                "the client left the stream; closing the upstream connection"
            );
        }
    }
}

/// Where a streamed request body reports the problems it produced.
///
/// A streamed body cannot hand its own problem to the caller through the
/// transport error alone, so the guard records it and the dial reads it back.
#[derive(Clone, Default)]
struct BodyGuard(Arc<parking_lot::Mutex<Option<GatewayError>>>);

impl BodyGuard {
    /// Records the problem the body produced.
    fn record(&self, error: GatewayError) {
        *self.0.lock() = Some(error);
    }

    /// Takes the recorded problem, when the body produced one.
    fn take(&self) -> Option<GatewayError> {
        self.0.lock().take()
    }
}

/// The inbound request body, streamed upstream while it is still arriving.
struct RequestBodyStream {
    /// The inbound body, streaming.
    body: BodyDataStream,
    /// The framing the body is forwarded under.
    framing: BodyFraming,
    /// Where the body reports its own failures.
    guard: BodyGuard,
    /// The bytes streamed so far.
    total: usize,
}

impl RequestBodyStream {
    /// The limit the body is streamed under: its declared length, or the hard
    /// limit when the inbound request framed the body itself.
    fn limit(&self) -> usize {
        match self.framing {
            BodyFraming::Sized(declared) => declared,
            BodyFraming::Chunked => MAX_REQUEST_BODY_BYTES,
        }
    }

    /// The problem a body that leaves its frame produces (R10, R11).
    fn framing_error(&self, total: usize) -> GatewayError {
        match self.framing {
            BodyFraming::Sized(declared) => GatewayError::validation(
                format!(
                    "`content-length: {declared}` does not match the {total} bytes actually \
                     streamed"
                ),
                "content-length",
            ),
            BodyFraming::Chunked => payload_too_large(total),
        }
    }
}

impl Stream for RequestBodyStream {
    type Item = Result<Bytes, GatewayError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        match Pin::new(&mut this.body).poll_next(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                this.total = this.total.saturating_add(frame.len());

                if this.total > this.limit() {
                    let error = this.framing_error(this.total);
                    this.guard.record(error.clone());

                    return Poll::Ready(Some(Err(error)));
                }

                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                let problem = GatewayError::new(
                    GatewayErrorKind::Validation,
                    format!("the request body could not be read: {error}"),
                );
                this.guard.record(problem.clone());

                Poll::Ready(Some(Err(problem)))
            }
            // A declared length that was never reached is a mismatch (R10).
            Poll::Ready(None) => {
                let unreached =
                    matches!(this.framing, BodyFraming::Sized(declared) if this.total != declared);

                if unreached {
                    let error = this.framing_error(this.total);
                    this.guard.record(error.clone());

                    return Poll::Ready(Some(Err(error)));
                }

                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Maps a failure of the streamed upstream exchange to a problem (R15).
fn map_stream_error(error: hyper::Error, url: &str) -> GatewayError {
    if error.is_timeout() {
        return upstream_timeout(url);
    }

    unreachable_upstream(url, error)
}

/// 504 `cf.oagw.timeout.request.v1` (R15).
fn upstream_timeout(url: &str) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::UpstreamTimeout,
        format!("the upstream did not answer `{url}` within the configured timeout"),
    )
}

/// 502 for an upstream that could not be reached (R8, R15).
fn unreachable_upstream(url: &str, error: impl std::fmt::Display) -> GatewayError {
    GatewayError::new(
        GatewayErrorKind::DownstreamError,
        format!("the upstream at `{url}` could not be reached: {error}"),
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderValue, Request as ClientRequest, StatusCode};
    use futures_util::stream;
    use httpmock::{Method as MockMethod, MockServer};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::OagwConfig;
    use crate::domain::model::{PROTOCOL_HTTP, RouteSpec, UpstreamSpec};
    use crate::domain::store::ConfigService;
    use crate::error::ERROR_SOURCE_HEADER_NAME;
    use crate::proxy::register_proxy_routes;

    /// How long a test waits for a read before giving up on it.
    const READ_TIMEOUT: Duration = Duration::from_secs(5);

    /// The head of an answer that opens an SSE stream.
    const SSE_HEAD: &str =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\n";

    /// The alias every seeded upstream is given.
    const ALIAS: &str = "upstream";

    // -- harness ----------------------------------------------------------

    /// A gateway with plaintext upstreams allowed, so a raw socket can play the
    /// upstream.
    fn config() -> OagwConfig {
        OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        }
    }

    /// A data plane over an empty store; the store is returned so a test can
    /// seed it.
    fn data_plane() -> (Router, Arc<ConfigService>) {
        let config_service = Arc::new(ConfigService::new(config()));
        let service = Arc::new(ProxyService::new(config_service.clone()).unwrap());

        (
            register_proxy_routes(Router::new(), service),
            config_service,
        )
    }

    /// Seeds one plaintext upstream with the given endpoint and one route of it.
    fn seed(config_service: &ConfigService, port: u16, methods: &[&str], path: &str) {
        let spec: UpstreamSpec = serde_json::from_value(json!({
            "alias": ALIAS,
            "protocol": PROTOCOL_HTTP,
            "server": {
                "endpoints": [{ "host": "127.0.0.1", "port": port, "scheme": "http" }]
            }
        }))
        .unwrap();
        let created = config_service.create_upstream(Uuid::nil(), &spec).unwrap();

        let mut route_spec: RouteSpec = serde_json::from_value(json!({
            "match": { "http": { "methods": methods, "path": path } }
        }))
        .unwrap();
        route_spec.upstream_id = created.id;
        config_service
            .create_route(Uuid::nil(), &route_spec)
            .unwrap();
    }

    /// Serves a data plane on an ephemeral port, with one seeded plaintext
    /// upstream.
    async fn served_gateway(upstream_port: u16, methods: &[&str], path: &str) -> SocketAddr {
        let config_service = Arc::new(ConfigService::new(config()));
        seed(&config_service, upstream_port, methods, path);

        let service = Arc::new(ProxyService::new(config_service).unwrap());
        let router = register_proxy_routes(Router::new(), service);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        address
    }

    /// Serves exactly one raw upstream connection on an ephemeral port, handed
    /// to `handler` once the request head has been read.
    async fn serve_once<F, Fut>(handler: F) -> u16
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            handler(socket).await;
        });

        port
    }

    /// Reads and returns the head of a raw request, up to its blank line.
    async fn read_request_head(socket: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];

        while find(&buffer, b"\r\n\r\n").is_none() {
            let read = tokio::time::timeout(READ_TIMEOUT, socket.read(&mut chunk))
                .await
                .unwrap()
                .unwrap();
            buffer.extend_from_slice(&chunk[..read]);
        }

        String::from_utf8_lossy(&buffer).into_owned()
    }

    /// The offset of `marker` in `haystack`, when it is there.
    fn find(haystack: &[u8], marker: &[u8]) -> Option<usize> {
        haystack
            .windows(marker.len())
            .position(|window| window == marker)
    }

    async fn send(router: Router, request: ClientRequest<Body>) -> axum::http::Response<Body> {
        router.oneshot(request).await.unwrap()
    }

    fn gateway_request(
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Body,
    ) -> ClientRequest<Body> {
        let mut builder = ClientRequest::builder().method(method).uri(path);

        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        builder.body(body).unwrap()
    }

    async fn body_text(response: axum::http::Response<Body>) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();

        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn problem_type(response: axum::http::Response<Body>) -> String {
        let document = body_text(response).await;

        serde_json::from_str::<serde_json::Value>(&document).unwrap()["type"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn error_source(response: &axum::http::Response<Body>) -> String {
        response
            .headers()
            .get(ERROR_SOURCE_HEADER_NAME.as_str())
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    /// A raw client connection to the gateway, read through a buffer so a head
    /// and the body fragments behind it can be asserted separately.
    struct RawClient {
        socket: TcpStream,
        buffer: Vec<u8>,
    }

    impl RawClient {
        async fn connect(address: SocketAddr) -> Self {
            Self {
                socket: TcpStream::connect(address).await.unwrap(),
                buffer: Vec::new(),
            }
        }

        async fn send(&mut self, request: &str) {
            self.socket.write_all(request.as_bytes()).await.unwrap();
            self.socket.flush().await.unwrap();
        }

        /// Reads until `marker` has been seen, and returns what was read.
        async fn read_until(&mut self, marker: &str) -> String {
            let marker = marker.as_bytes();

            loop {
                if let Some(position) = find(&self.buffer, marker) {
                    let read = self
                        .buffer
                        .drain(..position + marker.len())
                        .collect::<Vec<u8>>();
                    return String::from_utf8_lossy(&read).into_owned();
                }

                let mut chunk = [0_u8; 1024];
                let read = tokio::time::timeout(READ_TIMEOUT, self.socket.read(&mut chunk))
                    .await
                    .expect("the read does not hang")
                    .expect("the read succeeds");

                assert!(
                    read > 0,
                    "the connection closed before `{}` was seen",
                    String::from_utf8_lossy(marker)
                );
                self.buffer.extend_from_slice(&chunk[..read]);
            }
        }

        /// Reads until the gateway closes the connection, and returns the rest.
        async fn read_to_end(&mut self) -> String {
            let mut rest = std::mem::take(&mut self.buffer);
            let mut chunk = [0_u8; 1024];

            loop {
                match tokio::time::timeout(READ_TIMEOUT, self.socket.read(&mut chunk)).await {
                    Ok(Ok(0) | Err(_)) | Err(_) => break,
                    Ok(Ok(read)) => rest.extend_from_slice(&chunk[..read]),
                }
            }

            String::from_utf8_lossy(&rest).into_owned()
        }
    }

    // -- framing ----------------------------------------------------------

    #[test]
    fn test_only_chunked_transfer_encoding_is_streamed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        assert_eq!(
            validate_request_framing(&headers).unwrap(),
            BodyFraming::Chunked
        );

        headers.insert(header::TRANSFER_ENCODING, HeaderValue::from_static("gzip"));
        let error = validate_request_framing(&headers).unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[test]
    fn test_a_declared_content_length_is_kept_as_declared() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("18"));
        assert_eq!(
            validate_request_framing(&headers).unwrap(),
            BodyFraming::Sized(18)
        );

        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_static("not-a-number"),
        );
        assert_eq!(
            validate_request_framing(&headers).unwrap_err().status(),
            400
        );
    }

    #[test]
    fn test_a_declared_content_length_beyond_the_hard_limit_is_a_413() {
        let mut headers = HeaderMap::new();
        let declared = (MAX_REQUEST_BODY_MB * 1024 * 1024) + 1;
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(declared));

        let error = validate_request_framing(&headers).unwrap_err();

        assert_eq!(error.status(), 413);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
        );
    }

    #[test]
    fn test_only_an_event_stream_is_relayed_as_events() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        assert!(is_sse(&headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        assert!(is_sse(&headers));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        assert!(!is_sse(&headers));

        assert!(!is_sse(&HeaderMap::new()));
    }

    // -- SSE relay --------------------------------------------------------

    #[tokio::test]
    async fn test_an_sse_stream_is_relayed_event_by_event() {
        let (continue_tx, mut continue_rx) = mpsc::channel::<()>(1);

        // The upstream writes one event, then waits to be told to write the
        // second one: if the gear buffered the whole stream, the first event
        // could not reach the caller before the second is written.
        let port = serve_once(move |mut socket| async move {
            read_request_head(&mut socket).await;
            socket.write_all(SSE_HEAD.as_bytes()).await.unwrap();
            socket.write_all(b"data: one\n\n").await.unwrap();
            socket.flush().await.unwrap();
            continue_rx.recv().await.unwrap();
            socket.write_all(b"data: two\n\n").await.unwrap();
            socket.flush().await.unwrap();
            // The upstream is done: its close ends the client response too.
        })
        .await;

        let address = served_gateway(port, &["GET"], "/events").await;
        let mut client = RawClient::connect(address).await;
        client
            .send("GET /oagw/v1/proxy/upstream/events HTTP/1.1\r\nhost: gateway\r\n\r\n")
            .await;

        let head = client.read_until("\r\n\r\n").await;
        assert!(head.contains("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("content-type: text/event-stream"), "{head}");
        assert!(head.contains("x-oagw-error-source: upstream"), "{head}");

        assert!(client.read_until("data: one").await.contains("data: one"));

        continue_tx.send(()).await.unwrap();

        // The upstream closed after the second event: the client response ends
        // with it, rather than hanging open (R2).
        let rest = client.read_to_end().await;
        assert!(rest.contains("data: two"), "{rest}");
    }

    #[tokio::test]
    async fn test_a_client_disconnect_closes_the_upstream_connection() {
        let (writes_tx, mut writes_rx) = mpsc::unbounded_channel::<Result<(), String>>();

        // The upstream streams forever and reports every write, so the test can
        // see the moment the gear stops listening.
        let port = serve_once(move |mut socket| async move {
            read_request_head(&mut socket).await;
            socket.write_all(SSE_HEAD.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();

            loop {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let written = socket.write_all(b"data: tick\n\n").await;
                socket.flush().await.ok();
                if writes_tx
                    .send(written.map_err(|error| error.to_string()))
                    .is_err()
                {
                    return;
                }
            }
        })
        .await;

        let address = served_gateway(port, &["GET"], "/events").await;
        let mut client = RawClient::connect(address).await;
        client
            .send("GET /oagw/v1/proxy/upstream/events HTTP/1.1\r\nhost: gateway\r\n\r\n")
            .await;
        client.read_until("\r\n\r\n").await;
        client.read_until("data: tick").await;

        // The caller walks away mid-stream.
        drop(client);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut relays = 0_u32;

        loop {
            let written = tokio::time::timeout_at(deadline, writes_rx.recv())
                .await
                .expect("the upstream keeps writing until the gear closes the tunnel")
                .expect("the upstream reports its writes");

            if written.is_err() {
                break;
            }

            relays += 1;
        }

        assert!(
            relays > 0,
            "the upstream was written to before the disconnect"
        );
    }

    #[tokio::test]
    async fn test_a_mid_stream_failure_aborts_the_client_response() {
        let port = serve_once(move |mut socket| async move {
            read_request_head(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\n",
                )
                .await
                .unwrap();
            // A chunk that declares five bytes and stops after four, then a
            // connection cut in the middle of it.
            socket.write_all(b"5\r\ndata").await.unwrap();
            socket.flush().await.unwrap();
            drop(socket);
        })
        .await;

        let address = served_gateway(port, &["GET"], "/events").await;
        let mut client = RawClient::connect(address).await;
        client
            .send("GET /oagw/v1/proxy/upstream/events HTTP/1.1\r\nhost: gateway\r\n\r\n")
            .await;

        // The head was already on its way, so it is the upstream's answer.
        let head = client.read_until("\r\n\r\n").await;
        assert!(head.contains("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("x-oagw-error-source: upstream"), "{head}");

        // The relay aborts rather than closing the chunked framing cleanly.
        let rest = client.read_to_end().await;
        assert!(rest.contains("data"), "{rest}");
        assert!(
            !rest.contains("0\r\n"),
            "the abort is not a clean end: {rest}"
        );
    }

    // -- failures ---------------------------------------------------------

    #[tokio::test]
    async fn test_the_forwarded_request_target_is_origin_form() {
        // The upstream is an origin server, so the request line it receives is
        // origin-form (RFC 9112 §3.2.2): an absolute URI would leave it serving
        // `GET http://127.0.0.1:<port>/events` instead of `/events`, and a
        // request for a resource that exists would be answered with a 404. The
        // upstream below echoes the request line it was sent.
        let port = serve_once(move |mut socket| async move {
            let head = read_request_head(&mut socket).await;
            let request_line = head.lines().next().unwrap_or_default().to_owned();
            let body = format!("{request_line}|");

            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                         content-length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
        })
        .await;

        let address = served_gateway(port, &["GET"], "/events").await;
        let mut client = RawClient::connect(address).await;
        client
            .send("GET /oagw/v1/proxy/upstream/events HTTP/1.1\r\nhost: gateway\r\n\r\n")
            .await;

        let head = client.read_until("\r\n\r\n").await;
        assert!(head.contains("HTTP/1.1 200 OK"), "{head}");
        assert!(head.contains("x-oagw-error-source: upstream"), "{head}");

        let body = client.read_until("|").await;
        assert!(body.contains("GET /events HTTP/1.1"), "{body}");
        assert!(!body.contains("http://"), "{body}");
    }

    #[tokio::test]
    async fn test_an_unreachable_upstream_is_a_502_problem_before_the_head() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let (router, config_service) = data_plane();
        seed(&config_service, port, &["GET"], "/events");

        let response = send(
            router,
            gateway_request("GET", "/oagw/v1/proxy/upstream/events", &[], Body::empty()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
        );
    }

    // -- streamed request body --------------------------------------------

    #[tokio::test]
    async fn test_a_streamed_request_body_is_forwarded_to_the_upstream() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(MockMethod::POST)
                .path("/v1/upload")
                .body("chunk-one-chunk-two");
            then.status(200).body("stored");
        });

        let (router, config_service) = data_plane();
        seed(&config_service, server.port(), &["POST"], "/v1/upload");

        let response = send(
            router,
            gateway_request(
                "POST",
                "/oagw/v1/proxy/upstream/v1/upload",
                &[("content-type", "text/plain"), ("content-length", "19")],
                Body::from_stream(stream::iter([
                    Ok::<Bytes, GatewayError>(Bytes::from_static(b"chunk-one-")),
                    Ok(Bytes::from_static(b"chunk-two")),
                ])),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(error_source(&response), "upstream");
        assert_eq!(body_text(response).await, "stored");
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn test_a_request_body_that_leaves_its_declared_frame_is_a_400() {
        let server = MockServer::start();
        let (router, config_service) = data_plane();
        seed(&config_service, server.port(), &["POST"], "/v1/upload");

        let response = send(
            router,
            gateway_request(
                "POST",
                "/oagw/v1/proxy/upstream/v1/upload",
                &[("content-length", "5")],
                Body::from_stream(stream::iter([Ok::<Bytes, GatewayError>(
                    Bytes::from_static(b"much-longer-than-five"),
                )])),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[tokio::test]
    async fn test_a_transfer_encoding_other_than_chunked_is_a_400_before_the_dial() {
        let (router, config_service) = data_plane();
        seed(&config_service, 1, &["POST"], "/v1/upload");

        let response = send(
            router,
            gateway_request(
                "POST",
                "/oagw/v1/proxy/upstream/v1/upload",
                &[("transfer-encoding", "gzip")],
                Body::empty(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[tokio::test]
    async fn test_an_unsupported_method_is_a_404_route_problem() {
        let (router, config_service) = data_plane();
        seed(&config_service, 1, &["GET"], "/events");

        let response = send(
            router,
            gateway_request(
                "TRACE",
                "/oagw/v1/proxy/upstream/events",
                &[],
                Body::empty(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(error_source(&response), "gateway");
        assert_eq!(
            problem_type(response).await,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }
}
