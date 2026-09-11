//! Upstream transport: DNS resolution with an SSRF guard, connection
//! establishment (plaintext over TCP, TLS through Pingora's connector) and a
//! streaming HTTP/1.1 exchange that also carries protocol upgrades.
//!
//! Bodies are never buffered in either direction: the request body is handed
//! to hyper as a stream and the response body is returned as one, which is
//! what makes SSE pass through event-by-event and keeps the 100 MB ceiling a
//! framing check rather than a memory reservation.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::body::Body;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use pingora_core::connectors::TransportConnector;
use pingora_core::upstreams::peer::HttpPeer;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config::{OagwConfig, SsrfPolicyConfig};
use crate::domain::error::OagwError;
use crate::domain::model::Endpoint;

/// Anything that can carry an HTTP/1.1 exchange.
trait ProxyIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> ProxyIo for T {}

/// An upstream response, still streaming.
pub struct UpstreamResponse {
    /// Status and headers.
    pub parts: http::response::Parts,
    /// Response body, unread.
    pub body: hyper::body::Incoming,
    /// Upgraded connection handle, present only on a `101` response.
    pub on_upgrade: Option<hyper::upgrade::OnUpgrade>,
}

impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamResponse")
            .field("status", &self.parts.status)
            .field("upgraded", &self.on_upgrade.is_some())
            .finish_non_exhaustive()
    }
}

/// Connects to upstream endpoints and performs the HTTP exchange.
pub struct UpstreamConnector {
    tls: TransportConnector,
    allow_http_upstream: bool,
    ssrf: SsrfPolicyConfig,
    connect_timeout: Duration,
    header_timeout: Duration,
}

impl UpstreamConnector {
    /// Build a connector from the gear configuration.
    #[must_use]
    pub fn new(config: &OagwConfig) -> Self {
        Self {
            tls: TransportConnector::new(None),
            allow_http_upstream: config.allow_http_upstream,
            ssrf: config.ssrf_policy.clone(),
            connect_timeout: config.connect_timeout(),
            header_timeout: config.proxy_timeout(),
        }
    }

    /// Resolve `endpoint` to its socket addresses, applying the SSRF policy.
    ///
    /// Every candidate is returned, in resolver order, so [`Self::connect`]
    /// can fail over between the addresses one hostname carries — the
    /// connection-level retry the PRD permits, as distinct from re-issuing the
    /// client's request.
    ///
    /// # Errors
    ///
    /// * `403` — a resolved address is blocked by the SSRF policy;
    /// * `502` — the host could not be resolved;
    /// * `504` — resolution exceeded the connect budget.
    pub async fn resolve(&self, endpoint: &Endpoint) -> Result<Vec<SocketAddr>, OagwError> {
        let host = endpoint.host.trim_matches(|c| c == '[' || c == ']');
        let lookup = tokio::time::timeout(
            self.connect_timeout,
            tokio::net::lookup_host((host.to_owned(), endpoint.port)),
        )
        .await
        .map_err(|_| {
            OagwError::connection_timeout(format!(
                "resolving '{host}' exceeded {}s",
                self.connect_timeout.as_secs()
            ))
        })?
        .map_err(|err| {
            OagwError::downstream(format!("could not resolve upstream host '{host}': {err}"))
        })?;

        let addresses: Vec<SocketAddr> = lookup.collect();
        if addresses.is_empty() {
            return Err(OagwError::downstream(format!(
                "upstream host '{host}' resolved to no addresses"
            )));
        }
        // Validate every candidate, then pick the first: a DNS answer that
        // mixes a public and a private address must not be usable at all
        // (DNS-rebinding defence, `cpt-cf-oagw-nfr-ssrf-protection`).
        for address in &addresses {
            self.check_ssrf(host, address.ip())?;
        }
        Ok(addresses)
    }

    fn check_ssrf(&self, host: &str, ip: IpAddr) -> Result<(), OagwError> {
        if !self.ssrf.enabled {
            return Ok(());
        }
        if self
            .ssrf
            .allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
        {
            return Ok(());
        }
        if self.ssrf.allow_private_networks {
            return Ok(());
        }
        if is_internal(ip) {
            return Err(OagwError::forbidden(format!(
                "upstream host '{host}' resolves to the internal address {ip}, which the SSRF \
                 policy blocks"
            )));
        }
        Ok(())
    }

    /// Open a transport connection to `endpoint`.
    ///
    /// # Errors
    ///
    /// * `403` — plaintext requested while `allow_http_upstream` is off, or an
    ///   SSRF-blocked address;
    /// * `502` — the connection failed;
    /// * `504` — the connection attempt timed out.
    async fn connect(&self, endpoint: &Endpoint) -> Result<Box<dyn ProxyIo>, OagwError> {
        if endpoint.is_plaintext() && !self.allow_http_upstream {
            return Err(OagwError::forbidden(format!(
                "plaintext upstream connections are disabled; endpoint '{}://{}:{}' cannot be \
                 reached until oagw.config.allow_http_upstream is enabled",
                endpoint.scheme, endpoint.host, endpoint.port
            )));
        }
        let addresses = self.resolve(endpoint).await?;
        let mut last_error = None;
        for address in &addresses {
            match self.connect_to(endpoint, *address).await {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    tracing::debug!(
                        target: "oagw.proxy",
                        host = %endpoint.host,
                        address = %address,
                        detail = %err.detail,
                        "upstream address unreachable; trying the next one"
                    );
                    last_error = Some(err);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            OagwError::downstream(format!(
                "upstream host '{}' resolved to no usable address",
                endpoint.host
            ))
        }))
    }

    async fn connect_to(
        &self,
        endpoint: &Endpoint,
        address: SocketAddr,
    ) -> Result<Box<dyn ProxyIo>, OagwError> {
        if endpoint.is_plaintext() {
            let stream = tokio::time::timeout(
                self.connect_timeout,
                tokio::net::TcpStream::connect(address),
            )
            .await
            .map_err(|_| {
                OagwError::connection_timeout(format!(
                    "connecting to {address} exceeded {}s",
                    self.connect_timeout.as_secs()
                ))
            })?
            .map_err(|err| {
                OagwError::downstream(format!("could not connect to {address}: {err}"))
            })?;
            let _ = stream.set_nodelay(true);
            return Ok(Box::new(stream));
        }

        let mut peer = HttpPeer::new(address, true, endpoint.host.clone());
        peer.options.connection_timeout = Some(self.connect_timeout);
        peer.options.total_connection_timeout = Some(self.connect_timeout);
        let stream = self.tls.new_stream(&peer).await.map_err(|err| {
            OagwError::downstream(format!(
                "TLS connection to '{}:{}' failed: {err}",
                endpoint.host, endpoint.port
            ))
        })?;
        Ok(Box::new(stream))
    }

    /// Send `request` to `endpoint` and return the response head plus a
    /// still-streaming body.
    ///
    /// # Errors
    ///
    /// See [`Self::connect`], plus `502` for a protocol error and `504` when
    /// the response head does not arrive within the proxy timeout.
    pub async fn send(
        &self,
        endpoint: &Endpoint,
        request: http::Request<Body>,
    ) -> Result<UpstreamResponse, OagwError> {
        let stream = self.connect(endpoint).await?;
        let io = TokioIo::new(stream);
        let (mut sender, connection) = http1::handshake(io).await.map_err(|err| {
            OagwError::protocol(format!(
                "HTTP/1.1 handshake with '{}' failed: {err}",
                endpoint.host
            ))
        })?;

        // `with_upgrades` keeps the connection future able to hand back the
        // raw transport after a 101, which is what WebSocket proxying needs.
        // The task lives as long as the response body is being read.
        tokio::spawn(async move {
            if let Err(err) = connection.with_upgrades().await {
                tracing::debug!(
                    target: "oagw.proxy",
                    error = %err,
                    "upstream connection closed"
                );
            }
        });

        let mut response = tokio::time::timeout(self.header_timeout, sender.send_request(request))
            .await
            .map_err(|_| {
                OagwError::request_timeout(format!(
                    "upstream '{}' did not answer within {}s",
                    endpoint.host,
                    self.header_timeout.as_secs()
                ))
            })?
            .map_err(|err| {
                OagwError::downstream(format!(
                    "upstream '{}' request failed: {err}",
                    endpoint.host
                ))
            })?;

        let on_upgrade = if response.status() == http::StatusCode::SWITCHING_PROTOCOLS {
            Some(hyper::upgrade::on(&mut response))
        } else {
            None
        };
        let (parts, body) = response.into_parts();
        Ok(UpstreamResponse {
            parts,
            body,
            on_upgrade,
        })
    }
}

/// `true` for addresses that are not routable on the public internet.
#[must_use]
pub fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // 100.64.0.0/10 — carrier-grade NAT.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                // 192.0.0.0/24 — IETF protocol assignments.
                || v4.octets()[0..3] == [192, 0, 0]
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique-local and fe80::/10 link-local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|v4| is_internal(IpAddr::V4(v4)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{UpstreamConnector, is_internal};
    use crate::config::{OagwConfig, SsrfPolicyConfig};
    use crate::domain::model::Endpoint;
    use std::net::IpAddr;

    fn endpoint(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn internal_address_classification() {
        for internal in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = internal.parse().expect("parses");
            assert!(is_internal(ip), "{internal} must be treated as internal");
        }
        for public in ["1.1.1.1", "8.8.8.8", "93.184.216.34", "2606:4700::1111"] {
            let ip: IpAddr = public.parse().expect("parses");
            assert!(!is_internal(ip), "{public} must be treated as public");
        }
    }

    #[tokio::test]
    async fn plaintext_is_refused_unless_the_flag_is_set() {
        let strict = OagwConfig {
            allow_http_upstream: false,
            ssrf_policy: SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            ..OagwConfig::default()
        };
        let connector = UpstreamConnector::new(&strict);
        let request = http::Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .expect("request");
        let err = connector
            .send(&endpoint("http", "127.0.0.1", 1), request)
            .await
            .expect_err("plaintext refused");
        assert_eq!(err.status, 403);
        assert!(err.detail.contains("allow_http_upstream"));
    }

    #[tokio::test]
    async fn the_ssrf_guard_blocks_a_loopback_endpoint_when_enabled() {
        let guarded = OagwConfig {
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicyConfig {
                enabled: true,
                allow_private_networks: false,
                allowed_hosts: Vec::new(),
            },
            ..OagwConfig::default()
        };
        let connector = UpstreamConnector::new(&guarded);
        let err = connector
            .resolve(&endpoint("http", "127.0.0.1", 8080))
            .await
            .expect_err("blocked");
        assert_eq!(err.status, 403);
    }

    #[tokio::test]
    async fn an_allowlisted_host_bypasses_the_ssrf_guard() {
        let guarded = OagwConfig {
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicyConfig {
                enabled: true,
                allow_private_networks: false,
                allowed_hosts: vec!["127.0.0.1".to_owned()],
            },
            ..OagwConfig::default()
        };
        let connector = UpstreamConnector::new(&guarded);
        connector
            .resolve(&endpoint("http", "127.0.0.1", 8080))
            .await
            .expect("allowlisted");
    }

    #[tokio::test]
    async fn a_disabled_ssrf_policy_permits_loopback() {
        let open = OagwConfig {
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            ..OagwConfig::default()
        };
        let connector = UpstreamConnector::new(&open);
        let addresses = connector
            .resolve(&endpoint("http", "127.0.0.1", 8080))
            .await
            .expect("permitted");
        assert_eq!(addresses.len(), 1);
        assert_eq!(addresses[0].port(), 8080);
    }

    #[tokio::test]
    async fn an_unresolvable_host_is_a_downstream_error() {
        let open = OagwConfig {
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            ..OagwConfig::default()
        };
        let connector = UpstreamConnector::new(&open);
        let err = connector
            .resolve(&endpoint("http", "oagw-nonexistent-host.invalid", 80))
            .await
            .expect_err("unresolvable");
        assert_eq!(err.status, 502);
    }

    #[tokio::test]
    async fn a_hostname_with_several_addresses_yields_them_all_for_failover() {
        let open = OagwConfig {
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            ..OagwConfig::default()
        };
        let connector = UpstreamConnector::new(&open);
        let addresses = connector
            .resolve(&endpoint("http", "localhost", 8080))
            .await
            .expect("resolved");
        assert!(
            !addresses.is_empty(),
            "every candidate address is a failover target"
        );
        assert!(addresses.iter().all(|address| address.port() == 8080));
    }

    #[tokio::test]
    async fn a_refused_connection_is_a_bad_gateway() {
        let open = OagwConfig {
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            ..OagwConfig::default()
        };
        let connector = UpstreamConnector::new(&open);
        // Port 1 on loopback: nothing listens, so connect() is refused fast.
        let request = http::Request::builder()
            .uri("/")
            .body(axum::body::Body::empty())
            .expect("request");
        let err = connector
            .send(&endpoint("http", "127.0.0.1", 1), request)
            .await
            .expect_err("refused");
        assert_eq!(err.status, 502);
    }
}
