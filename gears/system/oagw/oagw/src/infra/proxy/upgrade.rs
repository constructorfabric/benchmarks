//! WebSocket session relay
//! (`cpt-cf-oagw-flow-request-proxy-websocket-session`).
//!
//! The session is relayed with a raw HTTP/1.1 upgrade on both sides: the
//! client's upgraded socket, taken by the handler before the response is
//! returned, is spliced to the upstream's upgraded socket with
//! `copy_bidirectional`, so no buffer ever holds the conversation and either
//! close closes the other direction.
//!
//! The buffered/SSE leg keeps `toolkit_http::HttpClient`; the upgrade leg
//! cannot go through its tower stack, which has no upgrade handoff, so it
//! carries its own connector. No `wss` or `wt` transport is built by this
//! entry: the relay leg speaks the plaintext HTTP/1.1 upgrade.

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::upgrade::Upgraded;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use crate::domain::dto::{Endpoint, HeadersConfig};
use crate::domain::error::DomainError;
use crate::domain::headers::OutboundRequestHeaders;
use crate::domain::proxy::{
    ErrorSource, ProxyBody, ProxyContext, ProxyObservation, ProxyResponse, StreamEvent, StreamKind,
    StreamLifecycle,
};

/// The client's inbound upgrade, taken by the handler before it dispatches.
///
/// `hyper::upgrade::on(&mut request)` resolves only once the client's
/// handshake completes, so the value is a future the relay task awaits.
pub struct InboundUpgrade {
    /// The client's upgraded connection.
    pub client: Pin<Box<dyn Future<Output = Result<Upgraded, hyper::Error>> + Send>>,
}

/// The connector the upgrade leg connects to an upstream through.
#[derive(Clone)]
pub struct UpstreamClient {
    inner: Client<HttpConnector, Full<Bytes>>,
}

impl UpstreamClient {
    /// A plaintext HTTP/1.1 connector.
    #[must_use]
    pub fn new() -> Self {
        Self { inner: Client::builder(TokioExecutor::new()).build_http() }
    }

    /// The client the upgrade request is issued through.
    #[must_use]
    pub const fn client(&self) -> &Client<HttpConnector, Full<Bytes>> {
        &self.inner
    }
}

impl Default for UpstreamClient {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for UpstreamClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UpstreamClient(http/1.1 upgrade)")
    }
}

/// Open the upstream session, splice the two sockets, and return the `101`
/// response head
/// (`inst-rp-ws-1` .. `-10`).
///
/// # Errors
///
/// Returns a downstream domain error when the upstream cannot be reached or
/// refuses the session; the client session then fails with
/// `X-OAGW-Error-Source: gateway`.
#[allow(clippy::too_many_arguments)]
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-4
// `inst-rp-ws-4` .. `-10`: the upstream handshake and the splice — the `101`
// head decides, the two sockets are spliced with `copy_bidirectional`, and
// either close or failure is recorded on the lifecycle.
pub async fn relay(
    client: &UpstreamClient,
    context: &ProxyContext,
    endpoint: &Endpoint,
    outbound: OutboundRequestHeaders,
    config: Option<&HeadersConfig>,
    inbound: InboundUpgrade,
    trace_id: Option<String>,
) -> Result<ProxyResponse, DomainError> {
    let InboundUpgrade { client: client_upgrade } = inbound;
    let lifecycle = StreamLifecycle::shared();
    let target = crate::domain::headers::upstream_url(
        endpoint,
        &context.request_path(),
        context.query.as_deref(),
    )?;
    let mut builder = http::Request::builder()
        .method(context.method.as_str())
        .uri(target);
    for (name, value) in &outbound.forwarded {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder = builder.header(crate::domain::headers::HOST_HEADER, outbound.host.as_str());
    let request = builder
        .body(Full::new(context.body.clone()))
        .map_err(|error| upgrade_failure(error.to_string(), trace_id.clone()))?;

    let mut response = client.inner.request(request).await.map_err(|error| {
        lifecycle.record(StreamEvent::Refused);
        refused_failure(endpoint, &error.to_string(), trace_id.clone())
    })?;
    let status = response.status().as_u16();
    if status != http::StatusCode::SWITCHING_PROTOCOLS.as_u16() {
        lifecycle.record(StreamEvent::Refused);
        return Err(refused_failure(endpoint, &format!("the upstream answered {status}"), trace_id));
    }
    let upstream_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap_or("").to_owned()))
        .collect();
    let headers = crate::domain::headers::build_upgrade_response_headers(&upstream_headers, config);
    let upstream = hyper::upgrade::on(&mut response);

    let lifecycle_for_task = Arc::clone(&lifecycle);
    tokio::spawn(async move {
        let client_io = match client_upgrade.await {
            Ok(io) => io,
            Err(_) => {
                lifecycle_for_task.record(StreamEvent::Aborted);
                return;
            }
        };
        let upstream_io = match upstream.await {
            Ok(io) => io,
            Err(_) => {
                lifecycle_for_task.record(StreamEvent::Refused);
                return;
            }
        };
        lifecycle_for_task.record(StreamEvent::Open);
        let mut client_io = hyper_util::rt::TokioIo::new(client_io);
        let mut upstream_io = hyper_util::rt::TokioIo::new(upstream_io);
        match tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
            Ok(_) => lifecycle_for_task.record(StreamEvent::Close),
            Err(_) => {
                lifecycle_for_task.record(StreamEvent::Aborted);
            }
        }
    });

    Ok(ProxyResponse {
        status,
        headers,
        body: ProxyBody::Empty,
        source: ErrorSource::Upstream,
        stream: StreamKind::WebSocket,
        lifecycle,
        error: None,
        observation: ProxyObservation::default(),
    })
}

/// The error the client session fails with when the upstream refuses it.
///
/// The upstream's own body is never forwarded for a refused session: the
/// client session fails with a gateway error and the detail is curated here.
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-4
fn refused_failure(endpoint: &Endpoint, detail: &str, trace_id: Option<String>) -> DomainError {
    let _ = detail;
    DomainError::DownstreamError {
        upstream_id: Some(endpoint.host.clone()),
        host: Some(endpoint.host.clone()),
        path: None,
        trace_id,
        retriable: false,
    }
}

fn upgrade_failure(detail: String, trace_id: Option<String>) -> DomainError {
    let _ = detail;
    DomainError::DownstreamError {
        upstream_id: None,
        host: None,
        path: None,
        trace_id,
        retriable: false,
    }
}
