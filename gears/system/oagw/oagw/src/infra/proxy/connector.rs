//! Upstream dialing.
//!
//! The data plane dials one connection per hop and hands it to hyper's HTTP/1.1
//! client connection handshake, which is what makes both streaming (SSE) and
//! WebSocket upgrades work: the `Connection` future is spawned, so
//! `hyper::upgrade::on` can hand the socket over once the upstream answers
//! `101`.
//!
//! * plaintext dials go through `tokio::net::TcpStream`, and are subject to the
//!   `allow_http_upstream` gate and the SSRF screen;
//! * TLS dials go through pingora's `TransportConnector`, which carries the
//!   rustls configuration this crate's dependency set provides.
//!
//! There is no connection pool: a pooled connection would outlive
//! `proxy_timeout_secs`, and `docs/DESIGN.md` pins "no caching" as a data-plane
//! principle.
use std::sync::Arc;

use hyper_util::rt::TokioIo;
use pingora_core::connectors::TransportConnector;
use pingora_core::protocols::Stream;
use pingora_core::upstreams::peer::{HttpPeer, PeerOptions, Scheme};

use crate::domain::model::EndpointScheme;
use crate::infra::proxy::failure::ProxyFailure;
use crate::infra::proxy::ssrf::{
    DialRefusal, SsrfPolicy, plaintext_allowed, refusal_failure, screen_address,
};

/// An established upstream transport, ready for an HTTP/1.1 exchange.
#[derive(Debug)]
pub enum Connection {
    /// Plaintext TCP.
    Plain(TokioIo<tokio::net::TcpStream>),
    /// TLS, via pingora's rustls connector.
    Tls(TokioIo<Stream>),
}

impl Connection {
    /// Whether the connection was secured with TLS.
    #[must_use]
    pub const fn is_tls(&self) -> bool {
        matches!(self, Self::Tls(_))
    }
}

/// Dials upstream endpoints.
#[derive(Clone)]
pub struct UpstreamDialer {
    tls: Arc<TransportConnector>,
    policy: SsrfPolicy,
    allow_http_upstream: bool,
}

impl UpstreamDialer {
    /// Build a dialer with the given screening policy.
    #[must_use]
    pub fn new(
        tls: Arc<TransportConnector>,
        policy: SsrfPolicy,
        allow_http_upstream: bool,
    ) -> Self {
        Self {
            tls,
            policy,
            allow_http_upstream,
        }
    }

    /// Screen, resolve and dial one endpoint.
    ///
    /// # Errors
    ///
    /// [`ProxyFailure`] when the scheme is not dialable, the address is
    /// screened, DNS fails, or the socket cannot be established.
    pub async fn dial(
        &self,
        scheme: EndpointScheme,
        host: &str,
        port: u16,
    ) -> Result<Connection, ProxyFailure> {
        if !plaintext_allowed(scheme, self.allow_http_upstream) {
            return Err(refusal_failure(
                host,
                &DialRefusal::SchemeDisallowed {
                    host: host.to_owned(),
                },
            ));
        }
        let socket = resolve(host, port).await?;
        screen_address(socket.ip(), self.policy)
            .map_err(|refusal| refusal_failure(host, &refusal))?;

        match scheme {
            EndpointScheme::Http => {
                let stream = tokio::net::TcpStream::connect(socket)
                    .await
                    .map_err(|error| dial_failure(host, port, false, &error.to_string()))?;
                stream.set_nodelay(true).ok();
                Ok(Connection::Plain(TokioIo::new(stream)))
            }
            _ => {
                let peer = HttpPeer {
                    _address: pingora_core::protocols::l4::socket::SocketAddr::Inet(socket),
                    scheme: Scheme::HTTPS,
                    sni: host.to_owned(),
                    proxy: None,
                    client_cert_key: None,
                    group_key: 0,
                    options: PeerOptions::new(),
                };
                let stream = self
                    .tls
                    .new_stream(&peer)
                    .await
                    .map_err(|error| dial_failure(host, port, true, &error.to_string()))?;
                Ok(Connection::Tls(TokioIo::new(stream)))
            }
        }
    }
}

/// Resolve a host into a socket address.
///
/// IP literals are parsed directly, so the SSRF screen sees the literal rather
/// than whatever DNS would say about it.
async fn resolve(host: &str, port: u16) -> Result<std::net::SocketAddr, ProxyFailure> {
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(address, port));
    }
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| {
            ProxyFailure::link_unavailable(format!(
                "cannot resolve upstream host '{host}': {error}"
            ))
        })?
        .next()
        .ok_or_else(|| {
            ProxyFailure::link_unavailable(format!("upstream host '{host}' resolves to no address"))
        })
}

/// The 503 a failed dial produces.
fn dial_failure(host: &str, port: u16, tls: bool, detail: &str) -> ProxyFailure {
    let transport = if tls { "tls" } else { "tcp" };
    ProxyFailure::link_unavailable(format!(
        "cannot establish the {transport} connection to {host}:{port}: {detail}"
    ))
}

#[cfg(test)]
#[path = "connector_tests.rs"]
mod tests;
