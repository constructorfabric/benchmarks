//! Upstream transport.
//!
//! Connections are opened through Pingora's `TransportConnector`, which owns
//! DNS-free dialling and the rustls handshake, and are then driven by a
//! hyper HTTP/1.1 client. Going through hyper (rather than Pingora's own
//! session types) is what makes protocol upgrades and byte-exact streaming
//! available on the same code path as a plain request.
//!
//! DNS resolution happens here rather than inside Pingora so that the
//! resolved address can be checked against the SSRF policy
//! (`cpt-cf-oagw-nfr-ssrf-protection`) *before* a socket is opened.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::body::Body;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use pingora_core::connectors::TransportConnector;
use pingora_core::upstreams::peer::HttpPeer;

use crate::config::SsrfPolicyConfig;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::Scheme;

/// A resolved, policy-approved upstream address.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    /// Hostname as configured — used for SNI and the `Host` header.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Socket address the connection will be opened to.
    pub addr: SocketAddr,
    /// Whether the connection is TLS-wrapped.
    pub tls: bool,
}

/// Reject an address the SSRF policy forbids.
///
/// # Errors
///
/// `400` naming the class of address that was refused.
pub fn check_address(policy: &SsrfPolicyConfig, host: &str, ip: IpAddr) -> DomainResult<()> {
    if !policy.enabled || policy.is_allowlisted(host) {
        return Ok(());
    }
    let refuse = |class: &str| {
        Err(DomainError::validation(format!(
            "endpoint '{host}' resolves to a {class} address, which the SSRF policy forbids"
        )))
    };
    if policy.block_loopback && ip.is_loopback() {
        return refuse("loopback");
    }
    if policy.block_link_local && is_link_local(ip) {
        return refuse("link-local");
    }
    if policy.block_private && is_private(ip) {
        return refuse("private");
    }
    if ip.is_unspecified() {
        return refuse("unspecified");
    }
    if is_multicast(ip) {
        return refuse("multicast");
    }
    Ok(())
}

fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        // fe80::/10
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64,
        // fc00::/7 unique local addresses
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

fn is_multicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_multicast(),
        IpAddr::V6(v6) => v6.is_multicast(),
    }
}

/// Opens vetted connections to upstream endpoints.
pub struct UpstreamConnector {
    transport: TransportConnector,
    ssrf_policy: SsrfPolicyConfig,
    allow_http: bool,
    connect_timeout: Duration,
}

impl UpstreamConnector {
    /// Build a connector for a deployment.
    #[must_use]
    pub fn new(
        ssrf_policy: SsrfPolicyConfig,
        allow_http: bool,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            transport: TransportConnector::new(None),
            ssrf_policy,
            allow_http,
            connect_timeout,
        }
    }

    /// Resolve `host:port` and check the result against the SSRF policy.
    ///
    /// # Errors
    ///
    /// * `400` when the scheme is plaintext and the deployment forbids it,
    ///   or when the address is refused by the SSRF policy.
    /// * `503` when the name does not resolve.
    pub async fn resolve(
        &self,
        scheme: Scheme,
        host: &str,
        port: u16,
    ) -> DomainResult<ResolvedEndpoint> {
        // `cpt-cf-oagw-constraint-https-only` is a *connect-time* gate: the
        // management API accepts a plaintext scheme as a legal field value,
        // and this is where the deployment decides whether such a connection
        // is actually opened.
        if !scheme.is_tls() && !self.allow_http {
            return Err(DomainError::validation(format!(
                "plaintext scheme '{}' is not permitted for upstream connections; \
                 set oagw.config.allow_http_upstream to enable it",
                scheme.as_str()
            )));
        }

        let addr = if let Ok(ip) = host.parse::<IpAddr>() {
            SocketAddr::new(ip, port)
        } else {
            let mut resolved = tokio::net::lookup_host((host, port)).await.map_err(|err| {
                DomainError::link_unavailable(format!("could not resolve '{host}': {err}"))
            })?;
            resolved.next().ok_or_else(|| {
                DomainError::link_unavailable(format!("'{host}' resolved to no addresses"))
            })?
        };

        check_address(&self.ssrf_policy, host, addr.ip())?;

        Ok(ResolvedEndpoint {
            host: host.to_owned(),
            port,
            addr,
            tls: scheme.is_tls(),
        })
    }

    /// Open a connection and hand back an HTTP/1.1 request sink.
    ///
    /// The returned `SendRequest` owns one connection; the driver future is
    /// spawned with `with_upgrades()` so a `101` response can hand the raw
    /// socket back for a WebSocket relay.
    ///
    /// # Errors
    ///
    /// `504` on connect timeout, `503` when the connection cannot be
    /// established, `502` when the HTTP handshake fails.
    pub async fn connect(&self, endpoint: &ResolvedEndpoint) -> DomainResult<SendRequest<Body>> {
        let peer = HttpPeer::new(endpoint.addr, endpoint.tls, endpoint.host.clone());

        let stream = tokio::time::timeout(self.connect_timeout, self.transport.new_stream(&peer))
            .await
            .map_err(|_| {
                DomainError::connection_timeout(format!(
                    "connecting to '{}:{}' exceeded {}s",
                    endpoint.host,
                    endpoint.port,
                    self.connect_timeout.as_secs()
                ))
            })?
            .map_err(|err| {
                DomainError::link_unavailable(format!(
                    "could not connect to '{}:{}': {err}",
                    endpoint.host, endpoint.port
                ))
            })?;

        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|err| {
                DomainError::protocol_error(format!(
                    "HTTP handshake with '{}:{}' failed: {err}",
                    endpoint.host, endpoint.port
                ))
            })?;

        let host = endpoint.host.clone();
        tokio::spawn(async move {
            if let Err(err) = connection.with_upgrades().await {
                tracing::debug!(
                    target: "oagw.proxy",
                    upstream_host = %host,
                    error = %err,
                    "upstream connection closed with an error"
                );
            }
        });

        Ok(sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict() -> SsrfPolicyConfig {
        SsrfPolicyConfig::default()
    }

    #[test]
    fn strict_policy_blocks_the_usual_suspects() {
        let policy = strict();
        for (label, ip) in [
            ("loopback", "127.0.0.1"),
            ("private", "10.0.0.1"),
            ("private", "192.168.1.1"),
            ("private", "172.16.0.1"),
            ("link-local metadata", "169.254.169.254"),
            ("unspecified", "0.0.0.0"),
        ] {
            let ip: IpAddr = ip.parse().expect("literal");
            assert!(
                check_address(&policy, "host.example", ip).is_err(),
                "{label} should be blocked"
            );
        }
    }

    #[test]
    fn strict_policy_permits_public_addresses() {
        assert!(
            check_address(&strict(), "api.openai.com", "203.0.113.10".parse().expect("ip")).is_ok()
        );
    }

    #[test]
    fn a_disabled_policy_permits_everything() {
        let policy = SsrfPolicyConfig {
            enabled: false,
            ..SsrfPolicyConfig::default()
        };
        assert!(check_address(&policy, "localhost", "127.0.0.1".parse().expect("ip")).is_ok());
    }

    #[test]
    fn the_allowlist_bypasses_the_address_checks() {
        let policy = SsrfPolicyConfig {
            allow_hosts: vec!["localhost".to_owned()],
            ..SsrfPolicyConfig::default()
        };
        assert!(check_address(&policy, "LOCALHOST", "127.0.0.1".parse().expect("ip")).is_ok());
        assert!(check_address(&policy, "other.host", "127.0.0.1".parse().expect("ip")).is_err());
    }

    #[test]
    fn ipv6_loopback_and_unique_local_are_blocked() {
        let policy = strict();
        assert!(check_address(&policy, "h", "::1".parse().expect("ip")).is_err());
        assert!(check_address(&policy, "h", "fd00::1".parse().expect("ip")).is_err());
        assert!(check_address(&policy, "h", "fe80::1".parse().expect("ip")).is_err());
    }

    #[tokio::test]
    async fn plaintext_is_refused_unless_the_deployment_opts_in() {
        let strict_connector = UpstreamConnector::new(
            SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            false,
            Duration::from_secs(1),
        );
        let err = strict_connector
            .resolve(Scheme::Http, "127.0.0.1", 8080)
            .await
            .expect_err("plaintext refused");
        assert_eq!(err.status(), 400);

        let permissive = UpstreamConnector::new(
            SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            true,
            Duration::from_secs(1),
        );
        let resolved = permissive
            .resolve(Scheme::Http, "127.0.0.1", 8080)
            .await
            .expect("plaintext permitted");
        assert!(!resolved.tls);
        assert_eq!(resolved.port, 8080);
    }

    #[tokio::test]
    async fn tls_schemes_are_always_permitted_by_the_scheme_gate() {
        let connector = UpstreamConnector::new(
            SsrfPolicyConfig {
                enabled: false,
                ..SsrfPolicyConfig::default()
            },
            false,
            Duration::from_secs(1),
        );
        let resolved = connector
            .resolve(Scheme::Https, "127.0.0.1", 443)
            .await
            .expect("tls permitted");
        assert!(resolved.tls);
    }
}
