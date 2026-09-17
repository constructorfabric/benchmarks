//! Outbound transport: pingora's connector feeding a per-request hyper/1.1
//! connection.
//!
//! `pingora` owns TLS, ALPN, the SSRF-checked socket connect and the connection
//! pool; `hyper` owns the HTTP/1.1 codec. The result is a plain
//! `(SendRequest<Body>, Connection)` pair the caller drives to completion.

use std::time::Duration;

use hyper_util::rt::TokioIo;
use pingora_core::connectors::TransportConnector;
use pingora_core::upstreams::peer::HttpPeer;

use crate::domain::dto::Endpoint;
use crate::domain::error::DomainError;

/// A resolved target: the pingora peer plus the addressing facts the response
/// path needs back.
#[derive(Debug, Clone)]
pub struct Target {
    /// Endpoint chosen for the connection.
    pub endpoint: Endpoint,
    /// Resolved address in `host:port` form.
    pub socket: std::net::SocketAddr,
    /// TLS is applied before any HTTP byte.
    pub tls: bool,
    /// SNI / TLS hostname.
    pub sni: String,
    /// Host header value.
    pub host_header: String,
    /// Effective port.
    pub port: u16,
}

impl Target {
    /// Resolve a target from an endpoint list, round-robin over the endpoints.
    ///
    /// # Errors
    /// [`DomainError::MissingTargetHost`] when the list is empty and
    /// [`DomainError::UnknownTargetHost`] when no address resolves.
    pub fn resolve(endpoints: &[Endpoint], offset: usize) -> Result<Self, DomainError> {
        let endpoint = endpoints
            .iter()
            .cycle()
            .nth(offset % endpoints.len().max(1))
            .ok_or(DomainError::MissingTargetHost)?;
        let host = endpoint.normalized_host();
        let port = endpoint.port();

        let socket = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            std::net::SocketAddr::new(ip, port)
        } else {
            use std::net::ToSocketAddrs;
            let first = format!("{host}:{port}")
                .to_socket_addrs()
                .map_err(|_| DomainError::UnknownTargetHost(host.clone()))?
                .next()
                .ok_or_else(|| DomainError::UnknownTargetHost(host.clone()))?;
            first
        };

        Ok(Self {
            tls: endpoint.scheme.needs_tls(),
            host_header: host.clone(),
            sni: host,
            socket,
            port,
            endpoint: endpoint.clone(),
        })
    }

    /// The pingora peer used to dial.
    #[must_use]
    pub fn peer(&self) -> HttpPeer {
        HttpPeer::new(self.socket, self.tls, self.sni.clone())
    }
}

/// Outbound connection factory.
pub struct OutboundTransport {
    connector: TransportConnector,
    connect_timeout: Duration,
}

impl std::fmt::Debug for OutboundTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundTransport")
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

impl OutboundTransport {
    /// Build a transport.
    #[must_use]
    pub fn new(connect_timeout: Duration) -> Self {
        let connector = TransportConnector::new(None);
        Self {
            connector,
            connect_timeout,
        }
    }

    /// Open a connection and run the HTTP/1.1 handshake.
    ///
    /// Returns the sender, a handle that drives the connection, and whether
    /// the socket is reusable afterwards.
    ///
    /// # Errors
    /// [`DomainError::ConnectionTimeout`], [`DomainError::LinkUnavailable`]
    /// and [`DomainError::ProtocolError`].
    pub async fn connect(
        &self,
        target: &Target,
    ) -> Result<
        (
            hyper::client::conn::http1::SendRequest<axum::body::Body>,
            ConnectionHandle,
        ),
        DomainError,
    > {
        let peer = target.peer();
        let stream = tokio::time::timeout(self.connect_timeout, self.connector.new_stream(&peer))
            .await
            .map_err(|_| DomainError::ConnectionTimeout)?
            .map_err(|err| {
                tracing::debug!(error = %err, target = %target.socket, "upstream connect failed");
                DomainError::LinkUnavailable(format!(
                    "connect to {} failed",
                    target.socket
                ))
            })?;

        let io = TokioIo::new(stream);
        let (sender, connection) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|err| DomainError::ProtocolError(format!("handshake failed: {err}")))?;
        let handle = ConnectionHandle::spawn(connection);
        Ok((sender, handle))
    }
}

/// Drives a hyper connection to completion in the background.
///
/// The task owns the upstream socket for as long as something is still being
/// read off it — a streamed response body or an upgraded tunnel — so the handle
/// is handed to whichever of the two is reading, and its `Drop` *detaches* the
/// driver rather than aborting it.
#[derive(Debug)]
pub struct ConnectionHandle {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ConnectionHandle {
    fn spawn(
        connection: hyper::client::conn::http1::Connection<
            TokioIo<pingora_core::protocols::Stream>,
            axum::body::Body,
        >,
    ) -> Self {
        let task = tokio::spawn(async move {
            // Upgrades are enabled so WebSocket bridging works.
            let _ = connection.with_upgrades().await;
        });
        Self { task: Some(task) }
    }

    /// Abort the driver task. Only for callers that know nothing is being read
    /// off the socket any more (a cancelled request).
    pub fn abort(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for ConnectionHandle {
    fn drop(&mut self) {
        // Detach, and deliberately *not* `abort()`: the driver pumps the
        // upstream response into the client, so aborting it as soon as the
        // request has been answered tears the socket away mid-stream and the
        // client receives a truncated body. Dropping the join handle detaches
        // the task; hyper closes the connection by itself once the request
        // sender and the body are gone, so nothing is leaked.
        let _ = self.task.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::EndpointScheme;

    #[test]
    fn resolve_prefers_the_first_endpoint() {
        // IP literals: resolution must not depend on the sandbox's DNS view.
        let endpoints = vec![
            Endpoint {
                scheme: EndpointScheme::Https,
                host: "203.0.113.10".into(),
                port: None,
            },
            Endpoint {
                scheme: EndpointScheme::Https,
                host: "203.0.113.11".into(),
                port: Some(8443),
            },
        ];
        let target = Target::resolve(&endpoints, 0).expect("resolved");
        assert_eq!(target.port, 443);
        assert!(target.tls);
        assert_eq!(target.host_header, "203.0.113.10");
        assert_eq!(target.socket.port(), 443);
    }

    #[test]
    fn resolve_rotates_by_offset() {
        let endpoints = vec![
            Endpoint {
                scheme: EndpointScheme::Https,
                host: "10.0.0.1".into(),
                port: Some(443),
            },
            Endpoint {
                scheme: EndpointScheme::Https,
                host: "10.0.0.2".into(),
                port: Some(443),
            },
        ];
        assert_eq!(
            Target::resolve(&endpoints, 1).expect("resolved").host_header,
            "10.0.0.2"
        );
        assert_eq!(
            Target::resolve(&endpoints, 2).expect("resolved").host_header,
            "10.0.0.1"
        );
    }

    #[test]
    fn ip_literals_resolve_without_dns() {
        let endpoints = vec![Endpoint {
            scheme: EndpointScheme::Http,
            host: "127.0.0.1".into(),
            port: Some(8080),
        }];
        let target = Target::resolve(&endpoints, 0).expect("resolved");
        assert_eq!(target.socket, "127.0.0.1:8080".parse().unwrap());
        assert!(!target.tls);
    }

    #[test]
    fn empty_endpoint_list_is_an_error() {
        assert!(Target::resolve(&[], 0).is_err());
    }
}
