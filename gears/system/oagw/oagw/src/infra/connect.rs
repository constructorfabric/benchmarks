//! Outbound connections to upstream services.
//!
//! One code path serves plain HTTP exchanges, server-sent-event streams and
//! WebSocket upgrades alike: a connection is opened (TLS when the scheme calls
//! for it), an HTTP/1.1 handshake is driven over it with upgrades enabled, and
//! the caller decides what to do with the response. Because the gateway relays
//! bytes after a `101` rather than interpreting frames, subprotocol
//! negotiation, ping/pong and close codes all propagate untouched.

use std::sync::Arc;

use hyper::body::Incoming;
use hyper::client::conn::http1;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::domain::error::DomainError;
use crate::domain::model::Scheme;

/// A live connection's request sender, plus the task driving it.
#[allow(missing_debug_implementations)]
pub struct Upstream {
    sender: http1::SendRequest<crate::infra::body::ProxyBody>,
}

fn unreachable(e: impl std::fmt::Display) -> DomainError {
    DomainError::UpstreamUnreachable {
        message: e.to_string(),
    }
}

/// Build the shared TLS client configuration, trusting the platform roots.
fn tls_config() -> Result<Arc<ClientConfig>, DomainError> {
    static CACHE: std::sync::OnceLock<Result<Arc<ClientConfig>, String>> =
        std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            let loaded = rustls_native_certs::load_native_certs();
            for cert in loaded.certs {
                // A certificate the store rejects is skipped rather than fatal.
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err("no platform trust roots are available".to_owned());
            }
            Ok(Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ))
        })
        .clone()
        .map_err(|e| DomainError::UpstreamUnreachable { message: e })
}

impl Upstream {
    /// Open a connection to `host:port` under `scheme` and complete the HTTP/1.1
    /// handshake, with upgrades enabled.
    ///
    /// # Errors
    /// Returns [`DomainError::UpstreamUnreachable`] when the transport or the
    /// handshake fails.
    // @cpt-begin:cpt-cf-oagw-dod-ph-timeout:p1:inst-full
    pub async fn connect(scheme: Scheme, host: &str, port: u16) -> Result<Self, DomainError> {
        let tcp = TcpStream::connect((host, port)).await.map_err(unreachable)?;
        // Proxying is latency-sensitive and the payloads are relayed rather
        // than accumulated, so Nagle only adds delay.
        let _ = tcp.set_nodelay(true);

        let sender = if scheme.is_tls() {
            let cfg = tls_config()?;
            let server_name = ServerName::try_from(host.to_owned())
                .map_err(|_| unreachable(format!("`{host}` is not a valid TLS server name")))?;
            let stream = TlsConnector::from(cfg)
                .connect(server_name, tcp)
                .await
                .map_err(unreachable)?;
            Self::handshake(TokioIo::new(stream)).await?
        } else {
            Self::handshake(TokioIo::new(tcp)).await?
        };

        Ok(Self { sender })
    }
    // @cpt-end:cpt-cf-oagw-dod-ph-timeout:p1:inst-full

    async fn handshake<I>(io: I) -> Result<http1::SendRequest<crate::infra::body::ProxyBody>, DomainError>
    where
        I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    {
        let (sender, conn) = http1::handshake(io).await.map_err(unreachable)?;
        // `with_upgrades` is what makes a 101 usable: without it the connection
        // task would not hand back the upgraded transport.
        tokio::spawn(async move {
            if let Err(e) = conn.with_upgrades().await {
                tracing::debug!(error = %e, "oagw upstream connection ended");
            }
        });
        Ok(sender)
    }

    /// Send a request over this connection.
    ///
    /// # Errors
    /// Returns [`DomainError::UpstreamUnreachable`] when the exchange fails.
    pub async fn send(
        &mut self,
        req: Request<crate::infra::body::ProxyBody>,
    ) -> Result<Response<Incoming>, DomainError> {
        self.sender.ready().await.map_err(unreachable)?;
        self.sender.send_request(req).await.map_err(unreachable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connecting_to_a_closed_port_is_an_unreachable_error() {
        // Port 1 on the loopback interface has nothing listening.
        let r = Upstream::connect(Scheme::Http, "127.0.0.1", 1).await;
        assert!(matches!(
            r.err(),
            Some(DomainError::UpstreamUnreachable { .. })
        ));
    }

    #[tokio::test]
    async fn an_unresolvable_host_is_an_unreachable_error() {
        let r = Upstream::connect(Scheme::Http, "no-such-host.invalid", 80).await;
        assert!(matches!(
            r.err(),
            Some(DomainError::UpstreamUnreachable { .. })
        ));
    }
}
