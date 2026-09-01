//! Outbound transport (DESIGN §3.2 "Gear Structure" — outbound connector).
//!
//! The connector owns the HTTP client used by the proxy engine. It is built on
//! the raw `hyper-util` legacy client instead of `toolkit-http` because the
//! gateway must not retry requests (DESIGN §2.1 "No automatic retries"), must
//! forward arbitrary HTTP methods and must stream response bodies through
//! unchanged.
//!
//! Review evidence (privilege boundary — egress):
//! * Guardrail: DESIGN §4.4 — HTTPS-only egress for the MVP.
//! * Rationale: `allow_http_upstream` is an explicit operator opt-in; the
//!   engine therefore refuses a plaintext `http://` target unless the caller
//!   has already admitted it.
//! * Validation performed: `ensure_allowed_scheme` unit test plus the
//!   `validate_endpoint_pool` control-plane rule.

use bytes::Bytes;
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use crate::domain::error::DomainError;
use crate::domain::models::{Endpoint, Upstream};

/// Side-channel header an auth plugin may use to add an outbound query
/// parameter.
///
/// The value is spliced into the request target by the transport and stripped
/// before the request is serialised, so it is never observable to the upstream
/// as a header and never logged.
pub const OUTBOUND_QUERY_HEADER: &str = "x-oagw-outbound-query";

/// Buffered request payload handed to the transport.
pub type OutboundBody = http_body_util::Full<Bytes>;

/// Concrete legacy client type used for outbound calls.
type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, OutboundBody>;

/// Buffered response of one upstream call.
pub struct UpstreamResponse {
    /// Status returned by the upstream.
    pub status: u16,
    /// Response headers, in arrival order.
    pub headers: Vec<(String, String)>,
    /// Body of the response.
    pub body: Incoming,
}

/// A request handed to the transport.
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    /// HTTP method.
    pub method: String,
    /// Absolute request target (`scheme://authority[/path][?query]`).
    pub url: String,
    /// Outbound headers, lowercased and in insertion order.
    pub headers: Vec<(String, String)>,
    /// Request payload.
    pub body: Bytes,
}

/// Outbound HTTP transport used by the proxy engine.
pub struct Transport {
    client: HttpClient,
    timeout: std::time::Duration,
}

impl Transport {
    /// Builds a transport with the caller's request budget.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::Internal`] when the platform trust store cannot be
    /// loaded, which would make every TLS target unverifiable.
    pub fn new(timeout: std::time::Duration) -> Result<Self, DomainError> {
        let mut http = HttpConnector::new();
        http.set_connect_timeout(Some(timeout));
        http.enforce_http(false);

        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|error| DomainError::Internal {
                detail: format!("outbound TLS trust store unavailable: {error}"),
            })?
            .https_or_http()
            .enable_all_versions()
            .wrap_connector(http);

        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_timer(TokioTimer::default());
        builder.pool_idle_timeout(std::time::Duration::from_mins(1));
        let client: HttpClient = builder.build(connector);

        Ok(Self { client, timeout })
    }

    /// Sends one request and returns the raw upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::RequestTimeout`] when the request exceeds the
    /// configured budget and [`DomainError::DownstreamError`] for every other
    /// transport failure.
    pub async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, DomainError> {
        let mut builder = http::Request::builder()
            .method(request.method.as_str())
            .uri(&request.url);
        for (name, value) in &request.headers {
            if name == OUTBOUND_QUERY_HEADER {
                continue;
            }
            builder = builder.header(name.as_str(), value.as_str());
        }
        let request = builder
            .body(http_body_util::Full::new(request.body))
            .map_err(|error| downstream(format!("invalid outbound request: {error}")))?;

        let response = tokio::time::timeout(self.timeout, self.client.request(request))
            .await
            .map_err(|_| DomainError::RequestTimeout {
                timeout_seconds: self.timeout.as_secs(),
                upstream_id: None,
            })?
            .map_err(|error| downstream(format!("upstream call failed: {error}")))?;

        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();

        Ok(UpstreamResponse {
            status,
            headers,
            body: response.into_body(),
        })
    }
}

/// Builds a `502 DownstreamError` for a transport failure.
fn downstream(detail: String) -> DomainError {
    DomainError::DownstreamError {
        detail,
        upstream_status: 502,
        upstream_id: None,
        host: None,
    }
}

/// Buffers a response body.
///
/// # Errors
///
/// Returns [`DomainError::StreamAborted`] when the body is interrupted
/// mid-flight; a partially received body is never re-served to the caller.
pub async fn read_body(body: Incoming) -> Result<bytes::Bytes, DomainError> {
    use http_body_util::BodyExt as _;
    body.collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(|error| DomainError::StreamAborted {
            detail: format!("upstream body aborted: {error}"),
            upstream_id: None,
        })
}

/// Renders the absolute request target of an endpoint.
#[must_use]
pub fn request_target(endpoint: &Endpoint, path: &str, query: Option<&str>) -> String {
    let scheme = if endpoint.scheme.is_tls() {
        "https"
    } else {
        "http"
    };
    with_query(format!("{scheme}://{}{path}", endpoint.host_with_port()), query)
}

/// Renders the absolute request target of an endpoint for WebSocket upgrades.
#[must_use]
pub fn websocket_target(endpoint: &Endpoint, path: &str, query: Option<&str>) -> String {
    let scheme = if endpoint.scheme.is_tls() {
        "wss"
    } else {
        "ws"
    };
    with_query(
        format!("{scheme}://{}{path}", endpoint.host_with_port()),
        query,
    )
}

/// Appends a query string when the request carries one.
fn with_query(mut url: String, query: Option<&str>) -> String {
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        url.push('?');
        url.push_str(query);
    }
    url
}

/// Validates the scheme of an outbound target.
///
/// # Errors
///
/// Returns [`DomainError::ProtocolError`] when the upstream is not reachable
/// over TLS and the operator has not allowed plaintext egress.
pub fn ensure_allowed_scheme(
    upstream: &Upstream,
    endpoint: &Endpoint,
    allow_http: bool,
) -> Result<(), DomainError> {
    if allow_http || endpoint.scheme.is_tls() {
        return Ok(());
    }
    Err(DomainError::ProtocolError {
        detail: format!(
            "upstream '{}' targets plaintext HTTP, which is disabled by policy",
            upstream.normalized_alias()
        ),
        upstream_id: Some(upstream.id),
        host: Some(endpoint.host.clone()),
    })
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;
