//! Endpoint selection, DNS resolution and the SSRF guard.
//!
//! This is where `cpt-cf-oagw-nfr-ssrf-protection` is enforced: names are
//! resolved here, the resulting addresses are validated *before* a connection
//! is attempted, and the address that passed validation is the one handed to
//! the connector — so there is no window between check and use in which DNS
//! could change its answer.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::{OagwConfig, SsrfPolicy};
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::model::{Endpoint, Upstream};
use crate::infra::metrics::SelectionMethod;

/// The endpoint chosen for one request, and how it was chosen.
#[derive(Debug, Clone)]
pub struct EndpointChoice {
    /// The selected endpoint.
    pub endpoint: Endpoint,
    /// How it was selected, for the routing metrics.
    pub selection: SelectionMethod,
}

/// Validate the `X-OAGW-Target-Host` value: a bare hostname or IP literal,
/// with no port, path or other decoration.
fn validate_target_host(raw: &str) -> OagwResult<String> {
    let value = raw.trim();
    let invalid = || {
        OagwError::new(
            ErrorKind::InvalidTargetHost,
            "X-OAGW-Target-Host must be a valid hostname or IP address (no port, path, or \
             special characters)",
        )
        .with("invalid_value", value.to_owned())
    };
    if value.is_empty() || value.len() > 253 {
        return Err(invalid());
    }
    if value.contains([':', '/', '?', '#', '@', ' ', '\\'])
        && !crate::domain::alias::is_ip_literal(value)
    {
        return Err(invalid());
    }
    if crate::domain::alias::is_ip_literal(value) {
        return Ok(crate::domain::alias::normalize_host(value));
    }
    crate::domain::alias::validate_host(value).map_err(|_| invalid())?;
    Ok(crate::domain::alias::normalize_host(value))
}

/// Pick the endpoint for a request.
///
/// # Errors
///
/// * `400 InvalidTargetHost` — the header is malformed.
/// * `400 UnknownTargetHost` — the header names no configured endpoint.
/// * `400 MissingTargetHost` — a multi-endpoint pool whose alias is a common
///   suffix cannot be disambiguated without the header.
pub fn select_endpoint(
    upstream: &Upstream,
    target_host: Option<&str>,
    round_robin: &AtomicUsize,
) -> OagwResult<EndpointChoice> {
    let endpoints = &upstream.server.endpoints;
    if endpoints.is_empty() {
        return Err(OagwError::internal(format!(
            "upstream {} has no endpoints",
            upstream.id
        )));
    }

    if let Some(raw) = target_host {
        let wanted = validate_target_host(raw)?;
        let found = endpoints
            .iter()
            .find(|endpoint| endpoint.normalized_host() == wanted);
        return match found {
            Some(endpoint) => Ok(EndpointChoice {
                endpoint: endpoint.clone(),
                selection: SelectionMethod::ExplicitHeader,
            }),
            None => Err(OagwError::new(
                ErrorKind::UnknownTargetHost,
                format!(
                    "X-OAGW-Target-Host '{wanted}' does not match any configured endpoint. \
                     Valid hosts: [{}]",
                    upstream.endpoint_hosts().join(", ")
                ),
            )
            .with("invalid_value", wanted)
            .with("valid_hosts", upstream.endpoint_hosts())),
        };
    }

    if endpoints.len() == 1 {
        return Ok(EndpointChoice {
            endpoint: endpoints[0].clone(),
            selection: SelectionMethod::Default,
        });
    }

    if upstream.requires_target_host() {
        return Err(OagwError::new(
            ErrorKind::MissingTargetHost,
            format!(
                "X-OAGW-Target-Host header required for multi-endpoint upstream with common \
                 suffix alias. Valid hosts: [{}]",
                upstream.endpoint_hosts().join(", ")
            ),
        )
        .with("alias", upstream.alias.clone())
        .with("valid_hosts", upstream.endpoint_hosts()));
    }

    let index = round_robin.fetch_add(1, Ordering::Relaxed) % endpoints.len();
    Ok(EndpointChoice {
        endpoint: endpoints[index].clone(),
        selection: SelectionMethod::RoundRobin,
    })
}

/// Whether an address is inside a range that must never be reachable through
/// an *outbound* gateway.
#[must_use]
pub fn is_internal_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.is_documentation()
                // 100.64.0.0/10 — carrier-grade NAT.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                // 0.0.0.0/8 and 240.0.0.0/4.
                || v4.octets()[0] == 0
                || v4.octets()[0] >= 240
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                // fc00::/7 unique-local and fe80::/10 link-local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped addresses inherit the IPv4 verdict.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_internal_address(IpAddr::V4(v4)))
        }
    }
}

/// Apply the SSRF policy to a resolved address.
///
/// # Errors
///
/// `400 Validation` when the address is refused.
pub fn check_ssrf(policy: &SsrfPolicy, host: &str, ip: IpAddr) -> OagwResult<()> {
    if !policy.enabled || policy.allow_private_networks {
        return Ok(());
    }
    if is_internal_address(ip) {
        return Err(OagwError::validation(format!(
            "host {host:?} resolves to {ip}, which is inside a network the outbound gateway \
             refuses to reach"
        ))
        .with("host", host.to_owned()));
    }
    Ok(())
}

/// Refuse a plaintext connection unless the deployment opted in.
///
/// The *field* accepts `http`/`ws` unconditionally — this is the second,
/// separate question of whether a plaintext connection is actually made, and
/// it is the only one `allow_http_upstream` governs.
///
/// # Errors
///
/// `400 Validation` when the endpoint is plaintext and the flag is off.
pub fn check_transport(config: &OagwConfig, endpoint: &Endpoint) -> OagwResult<()> {
    if endpoint.scheme.is_tls() || config.allow_http_upstream {
        return Ok(());
    }
    Err(OagwError::validation(format!(
        "endpoint {}://{}:{} is plaintext, and this deployment does not set \
         `oagw.config.allow_http_upstream`",
        endpoint.scheme.as_str(),
        endpoint.host,
        endpoint.port()
    )))
}

/// Resolve an endpoint to a single socket address, applying the SSRF policy.
///
/// # Errors
///
/// * `400 Validation` — every resolved address was refused by the policy.
/// * `502 DownstreamError` — the name does not resolve.
pub async fn resolve_endpoint(config: &OagwConfig, endpoint: &Endpoint) -> OagwResult<SocketAddr> {
    let host = endpoint.normalized_host();
    let port = endpoint.port();
    let bare = host.trim_start_matches('[').trim_end_matches(']');

    let addresses: Vec<IpAddr> = if let Ok(ip) = bare.parse::<IpAddr>() {
        vec![ip]
    } else {
        tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|err| {
                OagwError::new(
                    ErrorKind::DownstreamError,
                    format!("could not resolve upstream host {host:?}: {err}"),
                )
                .with("host", host.clone())
            })?
            .map(|addr| addr.ip())
            .collect()
    };

    if addresses.is_empty() {
        return Err(OagwError::new(
            ErrorKind::DownstreamError,
            format!("upstream host {host:?} resolved to no addresses"),
        )
        .with("host", host));
    }

    let mut last_error = None;
    for ip in addresses {
        match check_ssrf(&config.ssrf_policy, &host, ip) {
            Ok(()) => return Ok(SocketAddr::new(ip, port)),
            Err(err) => last_error = Some(err),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        OagwError::validation(format!("upstream host {host:?} has no usable address"))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Protocol, Scheme, ServerConfig};
    use uuid::Uuid;

    fn endpoint(host: &str) -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: host.to_owned(),
            port: Some(443),
        }
    }

    fn upstream(alias: &str, hosts: &[&str]) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            enabled: true,
            protocol: Protocol::Http,
            server: ServerConfig {
                endpoints: hosts.iter().map(|host| endpoint(host)).collect(),
            },
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
            seq: 0,
        }
    }

    #[test]
    fn single_endpoint_ignores_a_missing_header() {
        let upstream = upstream("api.openai.com", &["api.openai.com"]);
        let choice = select_endpoint(&upstream, None, &AtomicUsize::new(0)).unwrap();
        assert_eq!(choice.selection, SelectionMethod::Default);
        assert_eq!(choice.endpoint.host, "api.openai.com");
    }

    #[test]
    fn single_endpoint_validates_a_present_header() {
        let upstream = upstream("api.openai.com", &["api.openai.com"]);
        let choice =
            select_endpoint(&upstream, Some("api.openai.com"), &AtomicUsize::new(0)).unwrap();
        assert_eq!(choice.selection, SelectionMethod::ExplicitHeader);

        let err = select_endpoint(&upstream, Some("other.example.com"), &AtomicUsize::new(0))
            .expect_err("host is validated even when optional");
        assert_eq!(err.kind(), ErrorKind::UnknownTargetHost);
    }

    #[test]
    fn multi_endpoint_explicit_alias_round_robins() {
        // An IP-based pool cannot derive an alias, so the operator supplied
        // `my-service` and there is nothing ambiguous to disambiguate: the
        // request is load-balanced across the pool.
        let pool = upstream("my-service", &["10.0.1.1", "10.0.1.2"]);
        assert!(!pool.requires_target_host());

        let counter = AtomicUsize::new(0);
        let first = select_endpoint(&pool, None, &counter).unwrap();
        let second = select_endpoint(&pool, None, &counter).unwrap();
        assert_eq!(first.selection, SelectionMethod::RoundRobin);
        assert_ne!(first.endpoint.host, second.endpoint.host);

        // The header still bypasses load balancing when present.
        let pinned = select_endpoint(&pool, Some("10.0.1.2"), &counter).unwrap();
        assert_eq!(pinned.endpoint.host, "10.0.1.2");
        assert_eq!(pinned.selection, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn multi_endpoint_common_suffix_requires_the_header() {
        let upstream = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let err = select_endpoint(&upstream, None, &AtomicUsize::new(0))
            .expect_err("cannot disambiguate");
        assert_eq!(err.kind(), ErrorKind::MissingTargetHost);
        let problem = err.to_problem(None);
        assert_eq!(problem["valid_hosts"][0], "us.vendor.com");

        let choice =
            select_endpoint(&upstream, Some("eu.vendor.com"), &AtomicUsize::new(0)).unwrap();
        assert_eq!(choice.endpoint.host, "eu.vendor.com");
        assert_eq!(choice.selection, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn malformed_target_host_is_rejected() {
        let upstream = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        for bad in [
            "us.vendor.com:8443",
            "http://us.vendor.com",
            "us.vendor.com/x",
            "",
        ] {
            let err = select_endpoint(&upstream, Some(bad), &AtomicUsize::new(0))
                .expect_err("malformed target host");
            assert_eq!(err.kind(), ErrorKind::InvalidTargetHost, "{bad:?}");
        }
    }

    #[test]
    fn unknown_target_host_lists_the_valid_ones() {
        let upstream = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let err = select_endpoint(&upstream, Some("apac.vendor.com"), &AtomicUsize::new(0))
            .expect_err("unknown host");
        assert_eq!(err.kind(), ErrorKind::UnknownTargetHost);
        let problem = err.to_problem(None);
        assert_eq!(problem["invalid_value"], "apac.vendor.com");
        assert_eq!(problem["valid_hosts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn internal_ranges_are_recognised() {
        for internal in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "255.255.255.255",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(
                is_internal_address(internal.parse().unwrap()),
                "{internal} must be treated as internal"
            );
        }
        for external in ["1.1.1.1", "8.8.8.8", "93.184.216.34", "2606:4700::1111"] {
            assert!(
                !is_internal_address(external.parse().unwrap()),
                "{external} must be treated as external"
            );
        }
    }

    #[test]
    fn ssrf_guard_honours_the_policy() {
        let strict = SsrfPolicy {
            enabled: true,
            allow_private_networks: false,
        };
        assert!(check_ssrf(&strict, "localhost", "127.0.0.1".parse().unwrap()).is_err());
        assert!(check_ssrf(&strict, "example.com", "1.1.1.1".parse().unwrap()).is_ok());

        let disabled = SsrfPolicy {
            enabled: false,
            allow_private_networks: false,
        };
        assert!(check_ssrf(&disabled, "localhost", "127.0.0.1".parse().unwrap()).is_ok());

        let permissive = SsrfPolicy {
            enabled: true,
            allow_private_networks: true,
        };
        assert!(check_ssrf(&permissive, "localhost", "127.0.0.1".parse().unwrap()).is_ok());
    }

    #[test]
    fn plaintext_needs_the_flag() {
        let plaintext = Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port: Some(8080),
        };
        let strict = OagwConfig::default();
        assert!(check_transport(&strict, &plaintext).is_err());
        let relaxed = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        assert!(check_transport(&relaxed, &plaintext).is_ok());
        // A TLS endpoint never needs the flag.
        assert!(check_transport(&strict, &endpoint("api.openai.com")).is_ok());
    }

    #[tokio::test]
    async fn ip_literals_resolve_without_dns() {
        let config = OagwConfig {
            allow_http_upstream: true,
            ssrf_policy: SsrfPolicy {
                enabled: false,
                allow_private_networks: false,
            },
            ..OagwConfig::default()
        };
        let endpoint = Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port: Some(8080),
        };
        let addr = resolve_endpoint(&config, &endpoint).await.unwrap();
        assert_eq!(addr.to_string(), "127.0.0.1:8080");
    }

    #[tokio::test]
    async fn ssrf_policy_blocks_a_loopback_endpoint() {
        let config = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        let endpoint = Endpoint {
            scheme: Scheme::Http,
            host: "127.0.0.1".to_owned(),
            port: Some(8080),
        };
        let err = resolve_endpoint(&config, &endpoint)
            .await
            .expect_err("blocked");
        assert_eq!(err.kind(), ErrorKind::Validation);
    }
}
