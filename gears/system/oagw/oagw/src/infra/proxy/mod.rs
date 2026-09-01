//! HTTP proxy engine built on hyper-util (legacy client) + hyper-rustls.
//!
//! Supports:
//! * streaming request/response bodies (SSE — no buffering),
//! * protocol upgrades (WebSocket) via `hyper::upgrade::on` on both the
//!   client (upstream) and server sides,
//! * HTTP + HTTPS upstreams through a single TLS-aware connector,
//! * per-request connect+headers timeout (`proxy_timeout_secs`).

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, Uri};
use http_body_util::BodyExt;
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};

/// Boxed error used by the proxy bodies (the same box used by `toolkit-http`
/// responses): the hyper legacy client accepts any body whose error converts
/// into `Box<dyn Error + Send + Sync>`, so we never need to construct a
/// `hyper::Error` (whose constructor is crate-private) by hand.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Axum bodies are `UnsyncBoxBody` (not `Sync`), so the proxy bodies use the
/// unsync boxed variant end-to-end; the legacy hyper client only requires the
/// body to be `Send + Unpin`, which `UnsyncBoxBody` satisfies.
///
/// The error type is the boxed `BoxError` (the same box `toolkit-http`
/// responses use) so we never need to construct a `hyper::Error` (whose
/// constructor is crate-private) by hand.
pub type BoxBody = http_body_util::combinators::UnsyncBoxBody<Bytes, BoxError>;
pub type BoxStream = futures_util::stream::BoxStream<'static, Result<Bytes, BoxError>>;

/// Convert an `axum::Error` into the `BoxError` used by the proxy bodies.
#[must_use]
pub fn into_box_error(e: axum::Error) -> BoxError {
    e.into_inner()
}

/// Convert an upstream `hyper::Error` into the boxed error type.
#[must_use]
pub fn hyper_err_to_box(e: hyper::Error) -> BoxError {
    e.into()
}

/// Build a ready-made proxy body from a byte slice (used by tests / empty
/// bodies). `Full` bodies error with `Infallible`, which is erased into the
/// boxed error type.
pub fn boxed_bytes_body(bytes: impl Into<Bytes>) -> BoxBody {
    http_body_util::Full::new(bytes.into())
        .map_err(|never: std::convert::Infallible| -> BoxError { match never {} })
        .boxed_unsync()
}

/// Request handed to the engine.  `uri` carries the full target
/// (`scheme://host[:port]/path?query`); headers already have the hop-by-hop
/// filtering and Host replacement applied by the data plane.
#[derive(Debug)]
pub struct ProxyRequest {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
    pub body: BoxBody,
}

/// Response returned by the engine.  The body is a live stream — consumers
/// forward it chunk-by-chunk (SSE) or buffer it (bounded by config).
#[derive(Debug)]
pub struct ProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: BoxBody,
}

/// Result of an upgrade attempt.
pub enum UpgradeOutcome {
    /// The upstream agreed to upgrade (status 101).  The caller bridges the
    /// raw connection and must forward the upstream's 101 response headers —
    /// most importantly `Sec-WebSocket-Accept`, which the client cryptographically
    /// verifies against the key it sent (RFC 6455 §4.2.2).
    Upgraded {
        /// Upstream's 101 response headers (pre-hijack).
        headers: HeaderMap,
        /// Owned half of the upgraded connection.
        conn: hyper::upgrade::Upgraded,
    },
    /// Upstream refused (non-101) — pass the response through verbatim.
    Response(ProxyResponse),
}

/// Transport-level errors.  The data plane maps these onto the documented
/// OAGW error taxonomy (502/503/504).
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("request timeout after {0:?}")]
    RequestTimeout(Duration),
    #[error("connection error: {0}")]
    Connect(#[source] hyper_util::client::legacy::Error),
    #[error("stream aborted: {0}")]
    Stream(String),
    #[error("protocol upgrade failed: {0}")]
    Upgrade(String),
    #[error("uri error: {0}")]
    InvalidUri(String),
    #[error("body error: {0}")]
    Body(String),
}

/// Outbound proxy strategy.  `call` proxies a normal request (streaming both
/// ways); `upgrade` handles WebSocket-style protocol upgrades.
#[async_trait]
pub trait ProxyEngine: Send + Sync {
    async fn call(&self, req: ProxyRequest) -> Result<ProxyResponse, ProxyError>;
    async fn upgrade(&self, req: ProxyRequest) -> Result<UpgradeOutcome, ProxyError>;
}

/// Hyper-util legacy client engine with an injected clock-relevant timeout.
///
/// The connector is owned by the legacy client itself (which keeps the
/// connection pool warm across calls), so no extra keep-alive guard is
/// needed here.
pub struct HyperProxyEngine {
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        BoxBody,
    >,
    timeout: Duration,
}

impl HyperProxyEngine {
    /// Build the engine with the global proxy timeout from config.
    #[must_use]
    pub fn new(proxy_timeout_secs: u64) -> Self {
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .build(connector);
        Self {
            client,
            timeout: Duration::from_secs(proxy_timeout_secs.max(1)),
        }
    }

    async fn request(
        &self,
        req: ProxyRequest,
    ) -> Result<hyper::Response<hyper::body::Incoming>, ProxyError> {
        let mut request = hyper::Request::builder()
            .method(req.method)
            .uri(req.uri)
            .version(hyper::Version::HTTP_11)
            .body(req.body)
            .map_err(|e| ProxyError::InvalidUri(e.to_string()))?;
        *request.headers_mut() = req.headers;
        match tokio::time::timeout(self.timeout, self.client.request(request)).await {
            Ok(result) => result.map_err(ProxyError::Connect),
            Err(_) => Err(ProxyError::RequestTimeout(self.timeout)),
        }
    }
}

#[async_trait]
impl ProxyEngine for HyperProxyEngine {
    async fn call(&self, req: ProxyRequest) -> Result<ProxyResponse, ProxyError> {
        let resp = self.request(req).await?;
        let (parts, body) = resp.into_parts();
        Ok(ProxyResponse {
            status: parts.status,
            headers: parts.headers,
            body: body.map_err(hyper_err_to_box).boxed_unsync(),
        })
    }

    async fn upgrade(&self, req: ProxyRequest) -> Result<UpgradeOutcome, ProxyError> {
        let resp = self.request(req).await?;
        if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
            // Capture the 101 headers before `upgrade::on` consumes the
            // response — the data plane forwards them (esp. Sec-WebSocket-Accept).
            let headers = resp.headers().clone();
            match hyper::upgrade::on(resp).await {
                Ok(conn) => Ok(UpgradeOutcome::Upgraded { headers, conn }),
                Err(e) => Err(ProxyError::Upgrade(e.to_string())),
            }
        } else {
            let (parts, body) = resp.into_parts();
            Ok(UpgradeOutcome::Response(ProxyResponse {
                status: parts.status,
                headers: parts.headers,
                body: body.map_err(hyper_err_to_box).boxed_unsync(),
            }))
        }
    }
}

/// Wrapper used by the data plane to bridge an `Upgraded` connection into
/// `TokioIo` for bidirectional copying.
pub fn wrap_upgraded(upgraded: hyper::upgrade::Upgraded) -> TokioIo<hyper::upgrade::Upgraded> {
    TokioIo::new(upgraded)
}

/// Copy bytes in both directions until one side closes (WebSocket bridge).
///
/// Both endpoints must be `Unpin` (the data plane passes `TokioIo`-wrapped
/// upgraded connections).  `Unpin` input lets callers pass the connections by
/// value; the resulting future is `Send` because both hyper `Upgraded` and
/// `TokioIo` are `Send`.
///
/// # Errors
///
/// Returns the underlying I/O error when either side fails to read/write.
pub async fn pump_bidirectional<A, B>(a: A, b: B) -> std::io::Result<()>
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut a = a;
    let mut b = b;
    tokio::io::copy_bidirectional(&mut a, &mut b)
        .await
        .map(|_| ())
}
