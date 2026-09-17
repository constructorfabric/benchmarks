//! Outbound data-plane transport: real HTTP dialing over `hyper`.
//!
//! Phase 1 forwarded through `toolkit-http`, which buffers both directions.
//! The data plane needs three things that client cannot give it:
//!
//! * SSE pass-through — the upstream body must reach the caller *as it is
//!   produced*, so [`crate::domain::services::proxy_service::ProxyService`]
//!   needs the raw [`hyper::body::Incoming`] back instead of a `Vec<u8>`;
//! * WebSocket tunneling — the upgraded connection must stay open for the
//!   lifetime of the exchange (see [`UpstreamTransport::tunnel`]);
//! * HTTP/2 to upstreams — negotiated through TLS ALPN by the rustls
//!   connector (plaintext upstreams speak HTTP/1.1; `h2c` prior knowledge is
//!   not negotiated by hyper's pooled client).
//!
//! The client is pooled per `(scheme, authority)` and never retries: a failure
//! is surfaced to the caller immediately, as DESIGN.md requires.

use std::time::Duration;

use axum::body::Body as ChunkedBody;
use http::request::Builder as RequestBuilder;
use http::{Method, Request as HttpRequest, Uri};
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::{Client, ResponseFuture};
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::domain::error::DomainError;

/// Outbound request body: axum's chunked body is `http_body::Body<Data =
/// Bytes>`, `Send + Unpin`, cheap to build from an in-memory payload and
/// directly constructible from the inbound body — which is what makes a
/// WebSocket upgrade forwardable without re-buffering it.
pub type OutboundBody = ChunkedBody;

/// Either hyper client behind one send path.
enum OutboundClient<'a> {
    /// Plaintext pool (never used for `https` endpoints).
    Plain(&'a Client<HttpConnector, OutboundBody>),
    /// TLS + HTTP/1.1 + HTTP/2 (ALPN).
    Tls(&'a Client<HttpsConnector<HttpConnector>, OutboundBody>),
}

impl OutboundClient<'_> {
    fn request(&self, request: HttpRequest<OutboundBody>) -> ResponseFuture {
        match self {
            Self::Plain(client) => client.request(request),
            Self::Tls(client) => client.request(request),
        }
    }
}

/// Outbound transport for the data plane.
///
/// One plaintext client (always built — an upstream may declare `http`
/// endpoints and only the *policy* decides whether it is dialable) and one TLS
/// client, shared by every request. [`Clone`] is cheap: hyper's client is an
/// `Arc` around its connection pool.
#[derive(Clone)]
pub struct UpstreamTransport {
    plain: Client<HttpConnector, OutboundBody>,
    tls: Option<Client<HttpsConnector<HttpConnector>, OutboundBody>>,
    timeout: Duration,
}

impl std::fmt::Debug for UpstreamTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamTransport")
            .field("timeout", &self.timeout)
            .field("tls", &self.tls.is_some())
            .finish()
    }
}

impl UpstreamTransport {
    /// Builds the transport.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the rustls connector cannot be built
    /// (native root store unreadable).
    pub fn try_new(timeout: Duration) -> Result<Self, DomainError> {
        let plain = pooled(TokioExecutor::new()).build_http::<OutboundBody>();

        let tls = match Self::https_connector() {
            Ok(connector) => Some(
                pooled(TokioExecutor::new())
                    .build::<HttpsConnector<HttpConnector>, OutboundBody>(connector),
            ),
            // A missing root store must not stop the gear: plaintext upstreams
            // keep working and every `https` dial reports `LinkUnavailable`.
            Err(error) => {
                tracing::warn!(%error, "TLS connector unavailable; https upstreams will fail");
                None
            }
        };

        Ok(Self { plain, tls, timeout })
    }

    /// Builds the rustls connector with the native root store and both HTTP
    /// versions enabled (ALPN `h2`, `http/1.1`).
    fn https_connector() -> std::io::Result<HttpsConnector<HttpConnector>> {
        Ok(hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()?
            .https_or_http()
            .enable_all_versions()
            .build())
    }

    /// Sends one request, bounded by the proxy timeout.
    ///
    /// `plaintext` selects the connection pool: `true` dials `http`, `false`
    /// dials `https` (TLS, ALPN-negotiated HTTP/1.1 or HTTP/2). Whether a
    /// plaintext dial is *permitted* is the caller's policy decision.
    ///
    /// # Errors
    ///
    /// [`DomainError::RequestTimeout`] (504) when the upstream did not answer
    /// in time, [`DomainError::LinkUnavailable`] (503) when the connection
    /// could not be established, [`DomainError::ProtocolError`] (502) for a
    /// malformed transport exchange.
    pub async fn send(
        &self,
        alias: &str,
        plaintext: bool,
        request: HttpRequest<OutboundBody>,
    ) -> Result<http::Response<Incoming>, DomainError> {
        let future = self.client_for(alias, plaintext)?.request(request);
        let response = tokio::time::timeout(self.timeout, future)
            .await
            .map_err(|_| DomainError::RequestTimeout(alias.to_owned()))?
            .map_err(|error| map_client_error(alias, &error))?;
        Ok(response)
    }

    /// Sends one upgrade request and returns the outbound half of the tunnel.
    ///
    /// The caller takes the inbound half itself (it owns the inbound request)
    /// and splices the two with [`UpstreamTransport::tunnel`].
    ///
    /// # Errors
    ///
    /// The same errors as [`UpstreamTransport::send`], plus
    /// [`DomainError::ProtocolError`] when the upstream declines to switch
    /// protocols.
    pub async fn send_upgrade(
        &self,
        alias: &str,
        plaintext: bool,
        request: HttpRequest<OutboundBody>,
    ) -> Result<(http::Response<Incoming>, hyper::upgrade::OnUpgrade), DomainError> {
        let mut response = self.send(alias, plaintext, request).await?;
        if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
            return Err(DomainError::ProtocolError(
                alias.to_owned(),
                format!(
                    "upstream answered {} instead of switching protocols",
                    response.status().as_u16()
                ),
            ));
        }
        let outbound = hyper::upgrade::on(&mut response);
        Ok((response, outbound))
    }

    /// Splices an established inbound upgrade to the upstream one.
    ///
    /// Runs detached: the `101` response has already left the gateway, so a
    /// mid-tunnel failure can only be logged (never re-rendered).
    pub fn tunnel(
        inbound: hyper::upgrade::OnUpgrade,
        outbound: hyper::upgrade::OnUpgrade,
        alias: String,
    ) {
        tokio::spawn(async move {
            // `hyper::upgrade::Upgraded` implements hyper's `Read`/`Write`
            // traits; `TokioIo` adapts both halves to `tokio::io` so the
            // bidirectional splice is a single call.
            let (mut client_io, mut upstream_io) = match (inbound.await, outbound.await) {
                (Ok(client), Ok(upstream)) => (TokioIo::new(client), TokioIo::new(upstream)),
                (Err(error), _) | (_, Err(error)) => {
                    tracing::debug!(alias = %alias, %error, "upgrade handshake failed");
                    return;
                }
            };
            match tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
                Ok((sent, received)) => {
                    tracing::debug!(alias = %alias, sent, received, "upgrade tunnel closed");
                }
                Err(error) => {
                    tracing::debug!(alias = %alias, %error, "upgrade tunnel aborted");
                }
            }
        });
    }

    /// Resolves the client for a plaintext or TLS dial.
    fn client_for(&self, alias: &str, plaintext: bool) -> Result<OutboundClient<'_>, DomainError> {
        if plaintext {
            return Ok(OutboundClient::Plain(&self.plain));
        }
        match &self.tls {
            Some(tls) => Ok(OutboundClient::Tls(tls)),
            None => Err(DomainError::LinkUnavailable(
                alias.to_owned(),
                "TLS transport is unavailable in this build".to_owned(),
            )),
        }
    }
}

/// Shared connection-pool settings for both clients.
fn pooled(executor: TokioExecutor) -> hyper_util::client::legacy::Builder {
    let mut builder = hyper_util::client::legacy::Builder::new(executor);
    builder.pool_idle_timeout(Duration::from_secs(30));
    builder.pool_max_idle_per_host(16);
    builder
}

/// Parses an outbound target URI.
///
/// # Errors
///
/// [`DomainError::ProtocolError`] when the parts cannot form a URI.
pub fn build_uri(
    scheme: &str,
    authority: &str,
    path: &str,
    query: Option<&str>,
) -> Result<Uri, DomainError> {
    let raw = match query {
        Some(query) if !query.is_empty() => format!("{scheme}://{authority}{path}?{query}"),
        _ => format!("{scheme}://{authority}{path}"),
    };
    Uri::try_from(raw.as_str()).map_err(|error| {
        DomainError::ProtocolError(
            authority.to_owned(),
            format!("invalid upstream URI: {error}"),
        )
    })
}

/// Starts an outbound request builder.
#[must_use]
pub fn request_builder(method: &Method, uri: &Uri) -> RequestBuilder {
    HttpRequest::builder()
        .method(method.clone())
        .uri(uri.clone())
}

/// Maps a hyper client failure onto the OAGW taxonomy.
///
/// hyper's pooled client reports both dial failures and aborted exchanges as a
/// single opaque error type. A dial failure is [`DomainError::LinkUnavailable`]
/// (503); anything after the request left the gateway is a
/// [`DomainError::ProtocolError`] (502). OAGW never retries.
pub fn map_client_error(alias: &str, error: &hyper_util::client::legacy::Error) -> DomainError {
    if error.is_connect() {
        return DomainError::LinkUnavailable(alias.to_owned(), error.to_string());
    }
    DomainError::ProtocolError(
        alias.to_owned(),
        format!("upstream exchange failed: {error}"),
    )
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn builds_absolute_target_uris() {
        let uri = build_uri("http", "127.0.0.1:8099", "/v1/models", None).expect("uri");
        assert_eq!(uri.scheme_str(), Some("http"));
        assert_eq!(uri.host(), Some("127.0.0.1"));
        assert_eq!(uri.port_u16(), Some(8099));

        let uri = build_uri("https", "api.openai.com", "/v1", Some("a=1&b=2")).expect("uri");
        assert_eq!(uri.query(), Some("a=1&b=2"));
    }

    #[test]
    fn empty_query_is_dropped() {
        let uri = build_uri("http", "a.example", "/", Some("")).expect("uri");
        assert_eq!(uri.query(), None);
    }

    #[test]
    fn invalid_uris_are_protocol_errors() {
        let error = build_uri("http", "a example", "/x", None).unwrap_err();
        assert_eq!(error.http_status(), 502);
    }

    #[test]
    fn transport_is_debuggable_without_secrets() {
        let transport = UpstreamTransport::try_new(Duration::from_secs(2)).expect("transport");
        let rendered = format!("{transport:?}");
        assert!(rendered.contains("timeout"));
    }
}
