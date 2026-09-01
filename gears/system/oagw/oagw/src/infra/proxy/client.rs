//! Outbound transport: TLS-capable connector plus a dedicated HTTP/1.1 client.
//!
//! TLS is assembled from `pingora`'s rustls re-exports (a `tokio_rustls`
//! `TlsConnector` over a webpki trust store fed with the platform's native
//! certificates), because `hyper-rustls` and `tokio-rustls` are not direct
//! dependencies of this crate. The HTTP layer is `hyper`'s own
//! `client::conn::http1` handshake: it needs no service-trait dependency, keeps
//! `hyper::body::Incoming` response bodies streamed (never buffered) and,
//! with `with_upgrades`, hands back the *upstream* side of a `101 Switching
//! Protocols` exchange through `hyper::upgrade::on`.

use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::domain::error::DomainError;

/// The raw transport a proxied connection runs over.
#[derive(Debug)]
pub enum Transport {
    /// Cleartext TCP.
    Plain(Box<tokio::net::TcpStream>),
    /// TLS 1.2/1.3 over TCP (`pingora`'s re-export of `tokio_rustls`).
    Tls(Box<pingora_core::tls::ClientTlsStream<tokio::net::TcpStream>>),
}

impl AsyncRead for Transport {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(stream) => std::pin::Pin::new(stream.as_mut()).poll_read(cx, buf),
            Transport::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Transport {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Transport::Plain(stream) => std::pin::Pin::new(stream.as_mut()).poll_write(cx, buf),
            Transport::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(stream) => std::pin::Pin::new(stream.as_mut()).poll_flush(cx),
            Transport::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(stream) => std::pin::Pin::new(stream.as_mut()).poll_shutdown(cx),
            Transport::Tls(stream) => std::pin::Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

impl Transport {
    /// `true` when the transport speaks TLS.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        matches!(self, Transport::Tls(_))
    }

    /// Reads bytes, delegating to [`tokio::io::AsyncReadExt::read`].
    ///
    /// # Errors
    ///
    /// Propagates the transport I/O error.
    pub async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        tokio::io::AsyncReadExt::read(self, buf).await
    }

    /// Writes bytes, delegating to [`tokio::io::AsyncWriteExt::write`].
    ///
    /// # Errors
    ///
    /// Propagates the transport I/O error.
    pub async fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        tokio::io::AsyncWriteExt::write(self, buf).await
    }
}

/// TCP + optional TLS connector, shared by every outbound request.
#[derive(Clone)]
pub struct UpstreamConnector {
    tls: Arc<pingora_core::tls::TlsConnector>,
    allow_http: bool,
    connect_timeout: Duration,
    /// Budget for the whole connection establishment: TCP connect plus TLS
    /// handshake. A socket that accepts and then never speaks is dropped when
    /// the budget is spent, instead of hanging forever.
    establishment_timeout: Duration,
}

impl std::fmt::Debug for UpstreamConnector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamConnector")
            .field("allow_http", &self.allow_http)
            .field("connect_timeout_ms", &self.connect_timeout.as_millis())
            .field(
                "establishment_timeout_ms",
                &self.establishment_timeout.as_millis(),
            )
            .finish()
    }
}

impl UpstreamConnector {
    /// Builds a connector with the platform trust store.
    ///
    /// `connect_timeout` bounds the TCP connect; the same duration also bounds
    /// the whole `connect + TLS` establishment, so a server that accepts the
    /// socket and never speaks TLS cannot hold a request open indefinitely.
    #[must_use]
    pub fn new(allow_http: bool, connect_timeout: Duration) -> Self {
        Self {
            tls: Arc::new(Self::build_tls_connector()),
            allow_http,
            connect_timeout,
            establishment_timeout: connect_timeout,
        }
    }

    /// Overrides the overall `connect + TLS` establishment budget.
    pub fn with_establishment_timeout(&mut self, timeout: Duration) {
        self.establishment_timeout = timeout;
    }

    /// `true` when cleartext HTTP upstreams are permitted.
    #[must_use]
    pub fn allows_http(&self) -> bool {
        self.allow_http
    }

    fn build_tls_connector() -> pingora_core::tls::TlsConnector {
        let mut roots = pingora_core::tls::RootCertStore::empty();
        match pingora_core::tls::load_native_certs() {
            Ok(certs) => {
                for cert in certs {
                    let _ = roots.add(cert);
                }
            }
            Err(_) => tracing::warn!("no platform root certificates could be loaded"),
        }
        let config = pingora_core::tls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        pingora_core::tls::TlsConnector::from(Arc::new(config))
    }

    /// Opens a connection to `host:port`, upgrading to TLS for `https`.
    ///
    /// Both the TCP connect and the TLS handshake are bounded: the connect by
    /// `connect_timeout`, the whole establishment by
    /// `establishment_timeout`, so a socket that accepts and never speaks
    /// yields [`DomainError::ConnectionTimeout`] instead of hanging.
    ///
    /// # Errors
    ///
    /// [`DomainError::ProtocolError`] when cleartext HTTP is disabled,
    /// [`DomainError::ConnectionTimeout`] on connect or TLS timeout,
    /// [`DomainError::LinkUnavailable`] when the socket cannot be established
    /// and [`DomainError::ProtocolError`] on TLS handshake failure.
    pub async fn connect(
        &self,
        scheme: &str,
        host: &str,
        port: u16,
    ) -> Result<Transport, DomainError> {
        if scheme.eq_ignore_ascii_case("http") && !self.allow_http {
            return Err(DomainError::ProtocolError(
                "cleartext HTTP upstreams are disabled (allow_http_upstream = false)".to_owned(),
            ));
        }
        let established = tokio::time::Instant::now() + self.establishment_timeout;
        // An authority keeps the brackets around an IPv6 literal, but a dial
        // target must not carry them, so `[::1]:8080` dials `::1:8080`.
        let address = unbracketed(host);
        let socket = tokio::time::timeout(self.connect_timeout, async {
            tokio::net::TcpStream::connect((address, port)).await
        })
        .await
        .map_err(|_| {
            DomainError::ConnectionTimeout(format!("connecting to {host}:{port} timed out"))
        })?
        .map_err(|_| DomainError::LinkUnavailable {
            detail: format!("cannot connect to {host}:{port}"),
            retry_after_seconds: 1,
        })?;
        let _ = socket.set_nodelay(true);
        if scheme.eq_ignore_ascii_case("https") {
            let server_name =
                pingora_core::tls::ServerName::try_from(address.to_owned()).map_err(|_| {
                    DomainError::InvalidTargetHost(format!("'{host}' is not a valid TLS name"))
                })?;
            let tls = tokio::time::timeout_at(established, self.tls.connect(server_name, socket))
                .await
                .map_err(|_| {
                    DomainError::ConnectionTimeout(format!(
                        "TLS handshake with {host}:{port} timed out"
                    ))
                })?
                .map_err(|_| {
                    DomainError::ProtocolError(format!("TLS handshake with {host}:{port} failed"))
                })?;
            Ok(Transport::Tls(Box::new(tls)))
        } else {
            Ok(Transport::Plain(Box::new(socket)))
        }
    }

    /// The authority of `uri`, with the implicit default port filled in.
    #[must_use]
    pub fn authority(uri: &Uri) -> (String, String, u16) {
        let scheme = uri.scheme_str().unwrap_or("https").to_owned();
        let host = uri.host().unwrap_or_default().to_owned();
        let port = uri
            .port_u16()
            .unwrap_or(if scheme.eq_ignore_ascii_case("https") {
                443
            } else {
                80
            });
        (scheme, host, port)
    }
}

/// The dial target of an authority host: an IPv6 literal loses its brackets.
///
/// `http::Uri::host()` keeps them (`[::1]`), while neither the resolver nor a
/// TLS server name accepts the bracketed spelling.
#[must_use]
pub fn unbracketed(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host)
}

/// The origin-form request target of `uri` (RFC 9112 §3.2.2).
///
/// The gateway addresses an upstream *origin server*, not a proxy, so the
/// request line carries the path and query only and the authority travels in
/// the `Host` header. `hyper` writes absolute-form whenever the request URI
/// keeps its authority, which upstreams are not obliged to accept, so the
/// authority is stripped once the dial target has been read out of it.
#[must_use]
fn origin_form(uri: &Uri) -> Uri {
    let target = match uri.path_and_query() {
        Some(path_and_query) if !path_and_query.as_str().is_empty() => path_and_query.to_owned(),
        _ => "/".parse().expect("'/' is a valid path-and-query"),
    };
    Uri::builder()
        .path_and_query(target)
        .build()
        .unwrap_or_else(|_| Uri::from_static("/"))
}

/// The outbound HTTP client used by the Data Plane.
///
/// Every request gets its own connection: the gateway never multiplexes
/// unrelated client requests onto one upstream socket, which keeps streaming
/// responses and upgrades isolated.
#[derive(Clone, Debug)]
pub struct OutboundClient {
    connector: UpstreamConnector,
    handshake_timeout: Duration,
}

impl OutboundClient {
    /// Builds the client around a connector.
    ///
    /// `connect_timeout` bounds the TCP connect, the TLS handshake and the
    /// HTTP/1.1 handshake alike: each is a step of the same "reach the upstream"
    /// phase, and a server that accepts a socket and then stalls must yield a
    /// timeout instead of holding the request open forever.
    #[must_use]
    pub fn new(allow_http: bool, connect_timeout: Duration) -> Self {
        Self {
            connector: UpstreamConnector::new(allow_http, connect_timeout),
            handshake_timeout: connect_timeout,
        }
    }

    /// Overrides the HTTP/1.1 handshake budget.
    #[must_use]
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self.connector.with_establishment_timeout(timeout);
        self
    }

    /// The connector, reused by paths that need a raw transport.
    #[must_use]
    pub fn connector(&self) -> &UpstreamConnector {
        &self.connector
    }

    async fn open(&self, uri: &Uri) -> Result<TokioIo<Transport>, DomainError> {
        let (scheme, host, port) = UpstreamConnector::authority(uri);
        let transport = self.connector.connect(&scheme, &host, port).await?;
        Ok(TokioIo::new(transport))
    }

    /// Sends a request, streaming the response body.
    ///
    /// The returned response may carry a [`hyper::upgrade::OnUpgrade`]
    /// extension when the upstream answered `101 Switching Protocols`.
    ///
    /// # Errors
    ///
    /// [`DomainError::ProtocolError`] when the upstream connection cannot be
    /// established, [`DomainError::ConnectionTimeout`] when the HTTP/1.1
    /// handshake exceeds its budget, [`DomainError::PayloadTooLarge`] when the
    /// streamed request body exceeds its byte budget and
    /// [`DomainError::RequestTimeout`] when the response head is not received
    /// in time.
    pub async fn send(
        &self,
        request: http::Request<axum::body::Body>,
        timeout: Duration,
    ) -> Result<http::Response<Incoming>, DomainError> {
        let io = self.open(request.uri()).await?;
        let request = {
            let (mut parts, body) = request.into_parts();
            parts.uri = origin_form(&parts.uri);
            http::Request::from_parts(parts, body)
        };
        let (mut sender, connection) = tokio::time::timeout(
            self.handshake_timeout,
            hyper::client::conn::http1::handshake(io),
        )
        .await
        .map_err(|_| {
            DomainError::ConnectionTimeout(
                "the upstream did not complete the HTTP handshake in time".to_owned(),
            )
        })?
        .map_err(|_| {
            DomainError::ProtocolError("cannot establish the upstream connection".to_owned())
        })?;
        tokio::spawn(async move {
            if connection.with_upgrades().await.is_err() {
                tracing::debug!("upstream connection driver ended");
            }
        });
        tokio::time::timeout(timeout, sender.send_request(request))
            .await
            .map_err(|_| {
                DomainError::RequestTimeout("upstream did not respond in time".to_owned())
            })?
            .map_err(|error| {
                tracing::debug!(error = %error, "upstream request failed");
                if request_body_failure(&error) == Some(RequestBodyError::BudgetExceeded) {
                    return DomainError::PayloadTooLarge(
                        "the streamed request body exceeds the payload limit".to_owned(),
                    );
                }
                DomainError::DownstreamError("the upstream connection failed".to_owned())
            })
    }
}

/// Why the streamed request body failed while it was being sent upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestBodyError {
    /// The body grew past the payload budget while it was streaming.
    BudgetExceeded,
    /// The client disconnected or the transport broke mid-body.
    Aborted,
}

impl std::fmt::Display for RequestBodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::BudgetExceeded => "request payload exceeded the gateway limit",
            Self::Aborted => "the request body was aborted",
        })
    }
}

impl std::error::Error for RequestBodyError {}

/// Whether a hyper transport failure was caused by the request body itself.
///
/// A body that yields an error makes hyper fail the request; the marker is
/// reachable through the error's `source` chain.
#[must_use]
pub fn request_body_failure(error: &hyper::Error) -> Option<RequestBodyError> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(body_error) = current.downcast_ref::<RequestBodyError>() {
            return Some(*body_error);
        }
        source = current.source();
    }
    None
}

/// Reduces a hyper error to a non-sensitive classification.
#[must_use]
pub fn classify(error: &hyper::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_parse() {
        "protocol"
    } else if error.is_user() {
        "request"
    } else {
        "transport"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The authority of an outbound URI keeps the brackets of an IPv6 literal,
    /// so the dial target has to drop them (F-029).
    #[test]
    fn an_ipv6_literal_is_dialled_without_its_brackets() {
        assert_eq!(unbracketed("[::1]"), "::1");
        assert_eq!(unbracketed("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(unbracketed("api.vendor.com"), "api.vendor.com");
        assert_eq!(unbracketed("::1"), "::1");
    }

    #[test]
    fn the_authority_of_a_uri_keeps_the_brackets() {
        let uri: Uri = "http://[::1]:8080/v1/feed".parse().unwrap();
        let (scheme, host, port) = UpstreamConnector::authority(&uri);
        assert_eq!(scheme, "http");
        assert_eq!(host, "[::1]");
        assert_eq!(port, 8080);
        assert_eq!(unbracketed(&host), "::1");
    }

    /// The gateway addresses an upstream origin server, so the request line
    /// carries origin form and the authority stays in the `Host` header.
    #[test]
    fn the_request_target_is_reduced_to_origin_form() {
        let uri: Uri = "http://127.0.0.1:9098/v1/feed?a=1".parse().unwrap();
        assert_eq!(
            origin_form(&uri),
            "/v1/feed?a=1"
                .parse::<Uri>()
                .expect("a valid path-and-query")
        );

        // No query, no path suffix and an empty path all collapse to '/'.
        let bare: Uri = "https://api.vendor.com".parse().unwrap();
        assert_eq!(origin_form(&bare), "/");
        let root: Uri = "https://api.vendor.com/".parse().unwrap();
        assert_eq!(origin_form(&root), "/");
    }
}
