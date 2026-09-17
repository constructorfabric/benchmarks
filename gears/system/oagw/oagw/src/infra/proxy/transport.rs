//! Outbound transport for the data plane.
//!
//! Uses a dedicated `hyper` HTTP/1.1 connection per upstream request instead
//! of `toolkit-http`, which buffers bodies, decompresses and retries — all of
//! which break transparent SSE and WebSocket proxying. TLS uses `rustls`
//! with the OS trust store.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use http::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::error::{ErrorKind, OagwError};

/// Where an upstream connection goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Endpoint scheme (`https`, `http`, `wss`, `ws`, `grpc`, `wt`).
    pub scheme: String,
    /// Resolved host (hostname or IP literal).
    pub host: String,
    /// TCP port.
    pub port: u16,
}

/// `Host:` header value for an endpoint (port omitted when standard).
#[must_use]
pub fn host_header(endpoint: &crate::domain::model::Endpoint) -> String {
    if endpoint.is_standard_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

impl Target {
    /// `host:port` form used for connection keys.
    #[must_use]
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// `true` when the transport must speak TLS.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        matches!(self.scheme.as_str(), "https" | "wss" | "wt" | "grpc")
    }

    /// `scheme://authority` for the outbound URI.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme_for_uri(), self.authority())
    }

    /// Scheme written into the outbound request URI.
    fn scheme_for_uri(&self) -> &str {
        match self.scheme.as_str() {
            "wss" => "https",
            "ws" => "http",
            "grpc" | "wt" => "https",
            other => other,
        }
    }
}

/// An established upstream connection.
pub struct UpstreamConnection {
    /// Ready HTTP/1.1 sender.
    pub sender: hyper::client::conn::http1::SendRequest<crate::infra::proxy::ProxyBody>,
    /// Connection task handle; must be driven for the sender to work.
    pub connection: tokio::task::JoinHandle<()>,
}

/// Shared TLS configuration.
#[derive(Clone)]
pub struct TlsSettings {
    /// Client TLS configuration.
    pub config: std::sync::Arc<rustls::ClientConfig>,
    /// Skip certificate verification (non-production escape hatch).
    pub insecure: bool,
}

impl std::fmt::Debug for TlsSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsSettings").field("insecure", &self.insecure).finish()
    }
}

impl TlsSettings {
    /// Build settings from the OS trust store.
    ///
    /// # Errors
    /// Returns an error when the crypto provider cannot be installed.
    pub fn from_native_roots(insecure: bool) -> Result<Self, OagwError> {
        let builder = rustls::ClientConfig::builder();
        let config = if insecure {
            finish_config(
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAnyServerCert)),
            )
        } else {
            let mut roots = rustls::RootCertStore::empty();
            let certs = rustls_native_certs::load_native_certs();
            for err in &certs.errors {
                tracing::warn!(error = %err, "loading a native root certificate failed");
            }
            for cert in &certs.certs {
                let _ = roots.add(cert.clone());
            }
            if roots.is_empty() {
                tracing::warn!("no native root certificates loaded; TLS upstreams will fail");
            }
            finish_config(builder.with_root_certificates(roots))
        };
        Ok(Self {
            config: std::sync::Arc::new(config),
            insecure,
        })
    }
}

fn finish_config(
    builder: rustls::ConfigBuilder<rustls::ClientConfig, rustls::client::WantsClientCert>,
) -> rustls::ClientConfig {
    let mut config = builder.with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config
}

/// Establish an upstream connection to `target`.
///
/// # Errors
/// Maps DNS / TCP / TLS / handshake failures to `LinkUnavailable` and
/// timeouts to `ConnectionTimeout`.
pub async fn connect(
    target: &Target,
    tls: Option<&TlsSettings>,
    connect_timeout: Duration,
    server_name: &str,
) -> Result<UpstreamConnection, OagwError> {
    let addr = format!("{}:{}", target.host, target.port);
    let dial = tokio::time::timeout(connect_timeout, TcpStream::connect(&addr));
    let tcp = match dial.await {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("failed to connect to {addr}: {err}"),
            )
            .with_ext("host", target.host.clone()));
        }
        Err(_) => {
            return Err(OagwError::new(
                ErrorKind::ConnectionTimeout,
                format!("connection to {addr} timed out"),
            )
            .with_ext("host", target.host.clone()));
        }
    };
    let _ = tcp.set_nodelay(true);

    let io: UpstreamIo = if target.is_tls() {
        let settings = tls.ok_or_else(|| {
            OagwError::new(
                ErrorKind::LinkUnavailable,
                "TLS transport is not configured for this gateway",
            )
        })?;
        let server_name = rustls_pki_types::ServerName::try_from(server_name.to_owned())
            .map_err(|_| {
                OagwError::new(
                    ErrorKind::InvalidTargetHost,
                    format!("'{server_name}' cannot be used as a TLS server name"),
                )
            })?;
        let connector = tokio_rustls::TlsConnector::from(settings.config.clone());
        let tls_stream = tokio::time::timeout(
            connect_timeout,
            connector.connect(server_name, tcp),
        )
        .await
        .map_err(|_| {
            OagwError::new(
                ErrorKind::ConnectionTimeout,
                format!("TLS handshake with {addr} timed out"),
            )
        })?
        .map_err(|err| {
            OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("TLS handshake with {addr} failed: {err}"),
            )
        })?;
        UpstreamIo::Tls(Box::new(TokioIo::new(tls_stream)))
    } else {
        UpstreamIo::Plain(TokioIo::new(tcp))
    };

    let (sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|err| {
            OagwError::new(
                ErrorKind::ProtocolError,
                format!("HTTP handshake with {addr} failed: {err}"),
            )
        })?;
    // `with_upgrades()` is what makes hyper fulfil the `OnUpgrade` future a
    // caller may hold after `send_request`; without it a `101` degrades to the
    // "low level API in use" error, see `client/conn/http1.rs::UpgradeableConnection`.
    let connection = tokio::spawn(async move {
        let mut connection = connection.with_upgrades();
        if let Err(err) = (&mut connection).await {
            tracing::debug!(error = %err, "upstream connection closed");
        }
    });
    Ok(UpstreamConnection {
        sender,
        connection,
    })
}

/// Upstream byte stream, plain TCP or TLS.
///
/// The TLS side is boxed: the tokio-rustls stream is over a kilobyte and it
/// would otherwise dominate the enum's size on every plaintext connection.
enum UpstreamIo {
    /// Plaintext TCP.
    Plain(TokioIo<TcpStream>),
    /// TLS-terminated TCP.
    Tls(Box<TokioIo<tokio_rustls::client::TlsStream<TcpStream>>>),
}

impl Read for UpstreamIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: ReadBufCursor<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_read(cx, buf),
            Self::Tls(io) => Pin::new(io).poll_read(cx, buf),
        }
    }
}

impl Write for UpstreamIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_write(cx, buf),
            Self::Tls(io) => Pin::new(io).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_flush(cx),
            Self::Tls(io) => Pin::new(io).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            Self::Plain(io) => Pin::new(io).poll_shutdown(cx),
            Self::Tls(io) => Pin::new(io).poll_shutdown(cx),
        }
    }
}

/// Performant no-op verifier used only when `allow_insecure_tls` is enabled.
#[derive(Debug)]
struct AcceptAnyServerCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer<'_>,
        _intermediates: &[rustls_pki_types::CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls_pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

/// Parse `path?query` into the outbound request target for `target`.
///
/// The result is an *origin-form* target (`/path?query`). HTTP/1.1 requires it
/// for ordinary requests and hyper writes the URI verbatim into the request
/// line, so an absolute-form URI would leak to the upstream as a literal path.
/// The authority travels in the explicit `Host` header instead, see
/// [`host_header`].
#[must_use]
pub fn build_uri(_target: &Target, path_and_query: &str) -> Uri {
    let path = if path_and_query.is_empty() {
        "/"
    } else if path_and_query.starts_with('/') {
        path_and_query
    } else {
        // A bare `path?query` target, e.g. from a rewritten prefix.
        let owned = format!("/{path_and_query}");
        return owned.parse().unwrap_or_else(|_| {
            Uri::builder()
                .path_and_query("/")
                .build()
                .unwrap_or_else(|_| Uri::from_static("/"))
        });
    };
    Uri::builder()
        .path_and_query(path)
        .build()
        .unwrap_or_else(|_| Uri::from_static("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_header_omits_standard_port() {
        let https = crate::domain::model::Endpoint {
            scheme: "https".to_owned(),
            host: "api.example.com".to_owned(),
            port: 443,
        };
        assert_eq!(host_header(&https), "api.example.com");
        let plain = crate::domain::model::Endpoint {
            scheme: "http".to_owned(),
            host: "localhost".to_owned(),
            port: 8080,
        };
        assert_eq!(host_header(&plain), "localhost:8080");
    }

    #[test]
    fn target_authority_and_tls() {
        let t = Target {
            scheme: "https".to_owned(),
            host: "api.openai.com".to_owned(),
            port: 443,
        };
        assert!(t.is_tls());
        assert_eq!(t.authority(), "api.openai.com:443");
        assert_eq!(t.origin(), "https://api.openai.com:443");
    }

    #[test]
    fn plaintext_target_is_not_tls() {
        let t = Target {
            scheme: "http".to_owned(),
            host: "localhost".to_owned(),
            port: 8080,
        };
        assert!(!t.is_tls());
        assert_eq!(t.origin(), "http://localhost:8080");
    }

    #[test]
    fn websocket_schemes_map_to_http_framing() {
        let t = Target {
            scheme: "wss".to_owned(),
            host: "example.com".to_owned(),
            port: 443,
        };
        assert!(t.is_tls());
        assert_eq!(t.scheme_for_uri(), "https");
    }

    #[test]
    fn build_uri_appends_path() {
        let t = Target {
            scheme: "http".to_owned(),
            host: "localhost".to_owned(),
            port: 8080,
        };
        assert_eq!(build_uri(&t, "/v1/x").path(), "/v1/x");
        assert_eq!(build_uri(&t, "/v1/x?a=1").query(), Some("a=1"));
    }
}
