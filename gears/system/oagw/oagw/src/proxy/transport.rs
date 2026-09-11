//! Outbound transport: TLS setup and the hyper HTTP/1.1 client.

use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, Method, Uri, Version};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;

use crate::domain::error::OagwError;

/// An established upstream connection.
///
/// The TLS variant is boxed: the client session is over a kilobyte against
/// the socket's forty, and this enum is passed around by value.
pub enum UpstreamIo {
    /// Plaintext TCP.
    Plain(tokio::net::TcpStream),
    /// TLS over TCP.
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
}

impl tokio::io::AsyncRead for UpstreamIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for UpstreamIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Self::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Shared TLS client configuration (native-free, webpki roots).
pub struct TlsContext {
    config: std::sync::OnceLock<Arc<rustls::ClientConfig>>,
}

impl TlsContext {
    /// Empty context; the config is built on first use.
    #[must_use]
    pub fn new() -> Self {
        Self {
            config: std::sync::OnceLock::new(),
        }
    }

    /// Resolves the process-wide crypto provider.
    fn provider() -> Result<Arc<rustls::crypto::CryptoProvider>, OagwError> {
        #[cfg(feature = "fips")]
        {
            rustls::crypto::CryptoProvider::get_default().cloned().ok_or_else(|| {
                OagwError::protocol_error(
                    "crypto provider not installed; call toolkit::bootstrap::init_crypto_provider()",
                )
            })
        }
        #[cfg(not(feature = "fips"))]
        {
            Ok(rustls::crypto::CryptoProvider::get_default()
                .cloned()
                .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider())))
        }
    }

    /// Builds the shared TLS client configuration.
    ///
    /// # Errors
    ///
    /// Propagates the crypto-provider failure in FIPS mode.
    pub fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, OagwError> {
        if let Some(config) = self.config.get() {
            return Ok(config.clone());
        }
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Self::provider()?;
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| OagwError::protocol_error(format!("TLS configuration failed: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let config = Arc::new(config);
        let _ = self.config.set(config.clone());
        Ok(config)
    }
}

impl Default for TlsContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Establishes a connection to an endpoint.
///
/// # Errors
///
/// Connection failure, timeout or TLS failure, mapped onto the OAGW problems.
pub async fn connect(
    tls: &TlsContext,
    host: &str,
    port: u16,
    tls_required: bool,
    timeout: Duration,
) -> Result<UpstreamIo, OagwError> {
    let addr = format!("{host}:{port}");
    let connect = tokio::net::TcpStream::connect(&addr);
    let stream = match tokio::time::timeout(timeout, connect).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            return Err(
                OagwError::downstream_error(format!("failed to connect to {addr}: {e}"))
                    .with_extension("host", host),
            );
        }
        Err(_) => {
            return Err(OagwError::connection_timeout(format!(
                "connection to {addr} timed out after {timeout:?}"
            ))
            .with_extension("host", host));
        }
    };
    stream.set_nodelay(true).ok();

    if !tls_required {
        return Ok(UpstreamIo::Plain(stream));
    }

    let config = tls.client_config()?;
    let server_name = rustls_pki_types::ServerName::try_from(host.to_owned())
        .map_err(|e| OagwError::protocol_error(format!("invalid TLS server name {host:?}: {e}")))?;
    let connector = tokio_rustls::TlsConnector::from(config);
    match tokio::time::timeout(timeout, connector.connect(server_name, stream)).await {
        Ok(Ok(tls_stream)) => Ok(UpstreamIo::Tls(Box::new(tls_stream))),
        Ok(Err(e)) => Err(OagwError::downstream_error(format!(
            "TLS handshake with {addr} failed: {e}"
        ))
        .with_extension("host", host)),
        Err(_) => Err(OagwError::connection_timeout(format!(
            "TLS handshake with {addr} timed out after {timeout:?}"
        ))
        .with_extension("host", host)),
    }
}

/// A forwarded HTTP response with a streaming body.
pub struct ProxiedResponse {
    /// Upstream status.
    pub status: http::StatusCode,
    /// Upstream version.
    pub version: Version,
    /// Upstream headers.
    pub headers: HeaderMap,
    /// Streaming body.
    pub body: hyper::body::Incoming,
}

/// Sends a buffered request over an established connection and returns the
/// response.
///
/// The connection is single-use: HTTP/1.1 keep-alive pooling is left to a
/// future iteration, matching the "no connection pooling in the data plane"
/// constraint of the MVP.
///
/// # Errors
///
/// Protocol-level failures surface as [`OagwError::protocol_error`].
pub async fn send_request(
    io: UpstreamIo,
    method: &Method,
    uri: &Uri,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
    timeout: Duration,
) -> Result<ProxiedResponse, OagwError> {
    let body = Full::new(bytes::Bytes::from(body.unwrap_or_default())).boxed();

    let mut builder = http::Request::builder()
        .method(method.clone())
        .uri(uri.clone())
        .version(Version::HTTP_11);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(body)
        .map_err(|e| OagwError::protocol_error(format!("failed to build upstream request: {e}")))?;

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .map_err(|e| OagwError::protocol_error(format!("upstream protocol error: {e}")))?;

    let driver = tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("upstream connection closed with error: {e}");
        }
    });

    let response = match tokio::time::timeout(timeout, sender.send_request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            return Err(OagwError::downstream_error(format!(
                "upstream request failed: {e}"
            )));
        }
        Err(_) => {
            return Err(OagwError::request_timeout(format!(
                "upstream did not respond within {timeout:?}"
            )));
        }
    };

    let (
        http::response::Parts {
            status,
            version,
            headers,
            ..
        },
        body,
    ) = response.into_parts();

    let proxied = ProxiedResponse {
        status,
        version,
        headers,
        body,
    };
    // The driver task owns the connection; it ends when the body is drained or
    // the response future is dropped. Detaching it keeps the body readable.
    std::mem::forget(driver);

    Ok(proxied)
}
