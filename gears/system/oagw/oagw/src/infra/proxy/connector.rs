//! Upstream connection establishment.
//!
//! DNS resolution is done here (rather than inside the connector) so the SSRF
//! guard can inspect the resolved address before a socket is opened
//! (`cpt-cf-oagw-nfr-ssrf-protection`), and so a resolution failure surfaces
//! as a typed gateway error instead of a panic.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use pingora_core::connectors::TransportConnector;
use pingora_core::protocols::Stream;
use pingora_core::upstreams::peer::{HttpPeer, ALPN};

use crate::config::{OagwConfig, SsrfPolicyConfig};
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::model::Endpoint;

/// Opens L4 (+TLS) connections to upstream endpoints.
pub struct UpstreamConnector {
    transport: TransportConnector,
    connect_timeout: Duration,
    allow_http_upstream: bool,
    ssrf: SsrfPolicyConfig,
}

impl UpstreamConnector {
    #[must_use]
    pub fn new(config: &OagwConfig) -> Self {
        Self {
            transport: TransportConnector::new(None),
            connect_timeout: config.connect_timeout(),
            allow_http_upstream: config.allow_http_upstream,
            ssrf: config.ssrf_policy.clone(),
        }
    }

    /// Resolve, screen and connect to `endpoint`.
    ///
    /// # Errors
    /// * `503 LinkUnavailable` — plaintext refused, DNS failure, SSRF block.
    /// * `504 ConnectionTimeout` — the connect budget elapsed.
    /// * `502 ProtocolError` — TLS or transport failure.
    pub async fn connect(&self, endpoint: &Endpoint) -> OagwResult<Stream> {
        if !endpoint.scheme.is_tls() && !self.allow_http_upstream {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                format!(
                    "plaintext upstream connections are disabled; endpoint '{}://{}:{}' cannot be \
                     reached (set oagw.allow_http_upstream to permit it)",
                    endpoint.scheme.as_str(),
                    endpoint.host,
                    endpoint.port
                ),
            ));
        }

        let addr = self.resolve(&endpoint.host, endpoint.port).await?;
        let mut peer = HttpPeer::new(addr, endpoint.scheme.is_tls(), endpoint.host.clone());
        {
            let opts = &mut peer.options;
            opts.connection_timeout = Some(self.connect_timeout);
            opts.total_connection_timeout = Some(self.connect_timeout);
            // OAGW speaks HTTP/1.1 to upstreams; SSE and protocol upgrades both
            // depend on owning the byte stream after the response header.
            opts.alpn = ALPN::H1;
        }

        match tokio::time::timeout(self.connect_timeout, self.transport.new_stream(&peer)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(err)) => Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                format!(
                    "could not connect to upstream '{}:{}': {err}",
                    endpoint.host, endpoint.port
                ),
            )),
            Err(_) => Err(OagwError::new(
                ErrorKind::ConnectionTimeout,
                format!(
                    "connecting to upstream '{}:{}' exceeded {}s",
                    endpoint.host,
                    endpoint.port,
                    self.connect_timeout.as_secs()
                ),
            )),
        }
    }

    async fn resolve(&self, host: &str, port: u16) -> OagwResult<SocketAddr> {
        let literal = host.trim_start_matches('[').trim_end_matches(']');
        let candidates: Vec<SocketAddr> = if let Ok(ip) = literal.parse::<IpAddr>() {
            vec![SocketAddr::new(ip, port)]
        } else {
            tokio::net::lookup_host((host, port))
                .await
                .map_err(|err| {
                    OagwError::new(
                        ErrorKind::LinkUnavailable,
                        format!("DNS resolution failed for '{host}': {err}"),
                    )
                })?
                .collect()
        };

        let first = candidates.first().copied().ok_or_else(|| {
            OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("DNS resolution for '{host}' returned no addresses"),
            )
        })?;

        // Every resolved address is screened, not just the one we dial: a
        // rebinding attack that mixes public and internal answers is refused
        // outright rather than raced.
        for addr in &candidates {
            check_ssrf(&self.ssrf, host, addr.ip())?;
        }
        Ok(first)
    }
}

/// Reject addresses that a proxied request must never reach.
///
/// # Errors
/// `503 LinkUnavailable` when the address class is blocked by policy.
pub fn check_ssrf(policy: &SsrfPolicyConfig, host: &str, ip: IpAddr) -> OagwResult<()> {
    if !policy.enabled {
        return Ok(());
    }
    if policy
        .allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host))
    {
        return Ok(());
    }
    let class = classify(ip);
    let blocked = match class {
        AddressClass::Loopback => !policy.allow_loopback,
        AddressClass::Private | AddressClass::LinkLocal => !policy.allow_private_networks,
        AddressClass::Unspecified | AddressClass::Multicast => true,
        AddressClass::Public => false,
    };
    if blocked {
        return Err(OagwError::new(
            ErrorKind::LinkUnavailable,
            format!(
                "upstream host '{host}' resolves to a blocked {} address",
                class.as_str()
            ),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressClass {
    Loopback,
    Private,
    LinkLocal,
    Unspecified,
    Multicast,
    Public,
}

impl AddressClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Private => "private",
            Self::LinkLocal => "link-local",
            Self::Unspecified => "unspecified",
            Self::Multicast => "multicast",
            Self::Public => "public",
        }
    }
}

/// Bucket an IP into the classes the SSRF policy reasons about.
#[must_use]
pub fn classify(ip: IpAddr) -> AddressClass {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                AddressClass::Loopback
            } else if v4.is_unspecified() {
                AddressClass::Unspecified
            } else if v4.is_multicast() || v4.is_broadcast() {
                AddressClass::Multicast
            } else if v4.is_link_local() {
                AddressClass::LinkLocal
            } else if v4.is_private() {
                AddressClass::Private
            } else {
                AddressClass::Public
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                AddressClass::Loopback
            } else if v6.is_unspecified() {
                AddressClass::Unspecified
            } else if v6.is_multicast() {
                AddressClass::Multicast
            } else {
                let seg = v6.segments();
                if seg[0] & 0xffc0 == 0xfe80 {
                    AddressClass::LinkLocal
                } else if seg[0] & 0xfe00 == 0xfc00 {
                    AddressClass::Private
                } else if let Some(v4) = v6.to_ipv4_mapped() {
                    classify(IpAddr::V4(v4))
                } else {
                    AddressClass::Public
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;

    fn policy(enabled: bool) -> SsrfPolicyConfig {
        SsrfPolicyConfig {
            enabled,
            allow_loopback: false,
            allow_private_networks: false,
            allowed_hosts: Vec::new(),
        }
    }

    #[test]
    fn address_classification() {
        assert_eq!(
            classify("127.0.0.1".parse().expect("ip")),
            AddressClass::Loopback
        );
        assert_eq!(
            classify("10.0.0.1".parse().expect("ip")),
            AddressClass::Private
        );
        assert_eq!(
            classify("192.168.1.1".parse().expect("ip")),
            AddressClass::Private
        );
        assert_eq!(
            classify("169.254.169.254".parse().expect("ip")),
            AddressClass::LinkLocal
        );
        assert_eq!(
            classify("8.8.8.8".parse().expect("ip")),
            AddressClass::Public
        );
        assert_eq!(classify("::1".parse().expect("ip")), AddressClass::Loopback);
        assert_eq!(
            classify("fd00::1".parse().expect("ip")),
            AddressClass::Private
        );
    }

    #[test]
    fn the_guard_blocks_internal_targets_when_enabled() {
        let p = policy(true);
        // The AWS/GCP metadata endpoint is the canonical SSRF target.
        let err = check_ssrf(&p, "metadata.internal", "169.254.169.254".parse().expect("ip"))
            .expect_err("blocked");
        assert_eq!(err.status(), 503);
        assert!(check_ssrf(&p, "api.openai.com", "8.8.8.8".parse().expect("ip")).is_ok());
    }

    #[test]
    fn a_disabled_guard_permits_loopback() {
        let p = policy(false);
        assert!(check_ssrf(&p, "localhost", "127.0.0.1".parse().expect("ip")).is_ok());
    }

    #[test]
    fn allowances_and_allowlists_are_honoured() {
        let mut p = policy(true);
        p.allow_loopback = true;
        assert!(check_ssrf(&p, "localhost", "127.0.0.1".parse().expect("ip")).is_ok());
        assert!(check_ssrf(&p, "internal", "10.1.2.3".parse().expect("ip")).is_err());

        let mut p = policy(true);
        p.allowed_hosts = vec!["internal".to_owned()];
        assert!(check_ssrf(&p, "internal", "10.1.2.3".parse().expect("ip")).is_ok());
    }

    #[tokio::test]
    async fn plaintext_is_refused_unless_explicitly_allowed() {
        let mut cfg = OagwConfig::default();
        cfg.ssrf_policy.enabled = false;
        let connector = UpstreamConnector::new(&cfg);
        let endpoint = Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port: 9,
        };
        let err = connector.connect(&endpoint).await.expect_err("refused");
        assert_eq!(err.status(), 503);
        assert!(err.detail.contains("plaintext"));
    }
}
