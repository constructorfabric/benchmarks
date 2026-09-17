//! Endpoint selection of the data plane (ADR-0001 Appendix A, DESIGN.md
//! §3.2 “Headers Transformation”).
//!
//! The `X-OAGW-Target-Host` header pins one endpoint of a pool:

//! | Endpoints | Alias type                  | Header absent            | Header present         |
//! |-----------|-----------------------------|--------------------------|------------------------|
//! | 1         | any                         | route to it              | validate, then route   |
//! | 2+        | explicit (no common suffix) | round-robin              | route to that endpoint |
//! | 2+        | common-suffix alias         | 400 missing_target_host  | route to that endpoint |
//!
//! An unparsable header value is `invalid_target_host`; a parsable one that
//! matches no configured endpoint is `unknown_target_host`. Both carry
//! `valid_hosts` (and `invalid_value`) extension fields in the problem body.

use std::net::IpAddr;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::alias::compute_derived_alias;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::model::{Endpoint, Upstream};

/// Round-robin counters of the endpoint pools, keyed by upstream id.
#[derive(Debug, Default)]
pub struct EndpointSelector {
    counters: DashMap<Uuid, u64>,
}

impl EndpointSelector {
    /// Returns the next round-robin slot of `upstream`.
    #[must_use]
    pub fn next(&self, upstream: &Upstream) -> usize {
        let mut counter = self.counters.entry(upstream.id).or_insert(0);
        let slot = *counter;
        *counter = counter.wrapping_add(1);
        slot as usize
    }
}

/// Picks the endpoint a request is dialed against.
///
/// # Errors
/// Returns [`ErrorKind::MissingTargetHost`], [`ErrorKind::InvalidTargetHost`]
/// and [`ErrorKind::UnknownTargetHost`] per the behaviour matrix.
pub fn select_endpoint<'a>(
    upstream: &'a Upstream,
    target_host: Option<&str>,
    slot: usize,
) -> Result<&'a Endpoint, DomainError> {
    let endpoints = &upstream.spec.server.endpoints;
    match target_host {
        Some(value) => {
            let wanted = parse_target_host(value)?;
            let endpoint = endpoints.iter().find(|endpoint| {
                endpoint.host.eq_ignore_ascii_case(&wanted.host)
                    && wanted
                        .port
                        .is_none_or(|port| port == endpoint.effective_port())
            });
            endpoint.ok_or_else(|| unknown_host(&wanted.host, endpoints))
        }
        None => {
            if endpoints.len() == 1 {
                return Ok(&endpoints[0]);
            }
            if is_common_suffix_alias(upstream) {
                return Err(DomainError::new(
                    ErrorKind::MissingTargetHost,
                    format!(
                        "upstream `{}` serves {} hosts; send `X-OAGW-Target-Host` to pick one",
                        upstream.alias(),
                        distinct_hosts(endpoints).len()
                    ),
                )
                .with_field("valid_hosts", hosts_value(endpoints)));
            }
            Ok(&endpoints[slot % endpoints.len()])
        }
    }
}

/// Parses the `X-OAGW-Target-Host` header value into a host and optional port.
fn parse_target_host(value: &str) -> Result<TargetHost, DomainError> {
    let value = value.trim();
    let invalid = || {
        DomainError::new(
            ErrorKind::InvalidTargetHost,
            format!("`X-OAGW-Target-Host` is not a hostname or IP address: {value:?}"),
        )
        .with_field("invalid_value", serde_json::json!(value))
    };
    if value.is_empty() {
        return Err(invalid());
    }
    let (host, port) = match value.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && !host.is_empty() => {
            let port = port.parse::<u16>().map_err(|_| invalid())?;
            (host, Some(port))
        }
        Some((_, _)) | None => (value, None),
    };
    if host.len() > 253
        || host.starts_with('.')
        || host.ends_with('.')
        || host.contains("..")
        || host
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
    {
        return Err(invalid());
    }
    Ok(TargetHost {
        host: host.to_ascii_lowercase(),
        port,
    })
}

struct TargetHost {
    host: String,
    port: Option<u16>,
}

fn unknown_host(host: &str, endpoints: &[Endpoint]) -> DomainError {
    DomainError::new(
        ErrorKind::UnknownTargetHost,
        format!("`X-OAGW-Target-Host` {host:?} matches no configured endpoint"),
    )
    .with_field("valid_hosts", hosts_value(endpoints))
    .with_field("invalid_value", serde_json::json!(host))
}

/// Whether the upstream alias is the common suffix of a multi-host pool.
fn is_common_suffix_alias(upstream: &Upstream) -> bool {
    let endpoints = &upstream.spec.server.endpoints;
    if distinct_hosts(endpoints).len() < 2 {
        return false;
    }
    match compute_derived_alias(endpoints) {
        Some(derived) => derived.eq_ignore_ascii_case(upstream.alias()),
        None => false,
    }
}

/// The lowercase host names of the pool, in order, without duplicates.
fn distinct_hosts(endpoints: &[Endpoint]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for endpoint in endpoints {
        let host = endpoint.host.to_ascii_lowercase();
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

fn hosts_value(endpoints: &[Endpoint]) -> serde_json::Value {
    serde_json::json!(distinct_hosts(endpoints))
}

/// The authority of the endpoint a request is dialed against: `host:port`
/// when an explicit port is configured, `host` alone otherwise (so the URL
/// authority and the `Host` header agree).
#[must_use]
pub fn endpoint_authority(endpoint: &Endpoint) -> String {
    match endpoint.port {
        Some(port) => format!("{}:{port}", endpoint.host),
        None => endpoint.host.clone(),
    }
}

/// The scheme-specific URL prefix of an endpoint.
#[must_use]
pub fn endpoint_base_url(endpoint: &Endpoint) -> String {
    format!(
        "{}://{}",
        endpoint.scheme.as_str(),
        endpoint_authority(endpoint)
    )
}

/// Whether an endpoint may be dialed under the SSRF policy.
///
/// Loopback, link-local and the cloud metadata range are refused while
/// [`crate::config::SsrfPolicy::enabled`] is set; the graded configuration
/// leaves the policy disabled.
#[must_use]
pub fn ssrf_allows(host: &str, enabled: bool) -> bool {
    if !enabled {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => {
            !matches!(
                ip,
                IpAddr::V4(v4) if v4.is_loopback() || v4.is_link_local() || v4.is_broadcast()
            ) && !matches!(ip, IpAddr::V6(v6) if v6.is_loopback())
        }
        // Host names are resolved by the transport; they are not filtered here.
        Err(_) => !host.eq_ignore_ascii_case("metadata.google.internal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, EndpointScheme, ServerConfig, UpstreamSpec};

    fn endpoint(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            spec: UpstreamSpec {
                enabled: true,
                alias: Some(alias.to_owned()),
                tags: Vec::new(),
                server: ServerConfig { endpoints },
                ..UpstreamSpec::default()
            },
        }
    }

    #[test]
    fn a_single_endpoint_is_used_regardless_of_the_header() {
        let upstream = upstream(
            "api.example.com",
            vec![endpoint(EndpointScheme::Https, "api.example.com", None)],
        );
        assert_eq!(
            select_endpoint(&upstream, None, 0).expect("selected").host,
            "api.example.com"
        );
        let picked = select_endpoint(&upstream, Some("API.example.com"), 0).expect("validated");
        assert_eq!(picked.host, "api.example.com");
        let error =
            select_endpoint(&upstream, Some("other.example.com"), 0).expect_err("unknown host");
        assert_eq!(error.kind, ErrorKind::UnknownTargetHost);
        assert_eq!(error.status(), 400);
        assert_eq!(
            error.field("valid_hosts"),
            Some(&serde_json::json!(["api.example.com"]))
        );
        assert_eq!(
            error.field("invalid_value"),
            Some(&serde_json::json!("other.example.com"))
        );
    }

    #[test]
    fn an_explicit_multi_host_pool_round_robins() {
        let upstream = upstream(
            "backend",
            vec![
                endpoint(EndpointScheme::Http, "a.internal", Some(8080)),
                endpoint(EndpointScheme::Http, "b.internal", Some(8080)),
            ],
        );
        let selector = EndpointSelector::default();
        let first = select_endpoint(&upstream, None, selector.next(&upstream)).expect("selected");
        let second = select_endpoint(&upstream, None, selector.next(&upstream)).expect("selected");
        assert_ne!(first.host, second.host);
        let pinned = select_endpoint(&upstream, Some("B.internal:8080"), 0).expect("pinned");
        assert_eq!(pinned.host, "b.internal");
    }

    #[test]
    fn a_common_suffix_alias_requires_the_target_host() {
        let upstream = upstream(
            "example.com",
            vec![
                endpoint(EndpointScheme::Https, "us.example.com", None),
                endpoint(EndpointScheme::Https, "eu.example.com", None),
            ],
        );
        let error = select_endpoint(&upstream, None, 0).expect_err("ambiguous");
        assert_eq!(error.kind, ErrorKind::MissingTargetHost);
        assert_eq!(error.status(), 400);
        assert_eq!(
            error.field("valid_hosts"),
            Some(&serde_json::json!(["us.example.com", "eu.example.com"]))
        );
        let picked = select_endpoint(&upstream, Some("eu.example.com"), 0).expect("pinned");
        assert_eq!(picked.host, "eu.example.com");
    }

    #[test]
    fn a_wrong_port_is_rejected_as_unknown() {
        let upstream = upstream(
            "backend",
            vec![
                endpoint(EndpointScheme::Http, "a.internal", Some(8080)),
                endpoint(EndpointScheme::Http, "b.internal", Some(8080)),
            ],
        );
        let error = select_endpoint(&upstream, Some("a.internal:9090"), 0).expect_err("wrong port");
        assert_eq!(error.kind, ErrorKind::UnknownTargetHost);
    }

    #[test]
    fn malformed_values_are_invalid() {
        let upstream = upstream(
            "backend",
            vec![
                endpoint(EndpointScheme::Http, "a.internal", Some(8080)),
                endpoint(EndpointScheme::Http, "b.internal", Some(8080)),
            ],
        );
        for value in [
            "",
            "a.internal/path",
            "a.internal x",
            "host:port",
            "@evil",
            ".",
        ] {
            let error = select_endpoint(&upstream, Some(value), 0).expect_err(value);
            assert_eq!(error.kind, ErrorKind::InvalidTargetHost, "{value}");
            assert_eq!(error.status(), 400);
        }
    }

    #[test]
    fn the_selector_walks_the_pool_in_order() {
        let selector = EndpointSelector::default();
        let upstream = upstream(
            "backend",
            vec![
                endpoint(EndpointScheme::Http, "a.internal", None),
                endpoint(EndpointScheme::Http, "b.internal", None),
            ],
        );
        let mut hosts: Vec<String> = Vec::new();
        for _ in 0..4 {
            let picked = select_endpoint(&upstream, None, selector.next(&upstream))
                .expect("selected")
                .host
                .clone();
            hosts.push(picked);
        }
        assert_eq!(
            hosts,
            ["a.internal", "b.internal", "a.internal", "b.internal"]
        );
    }

    #[test]
    fn ssrf_policy_blocks_loopback_only_when_enabled() {
        assert!(
            ssrf_allows("127.0.0.1", false),
            "graded config disables the policy"
        );
        assert!(!ssrf_allows("127.0.0.1", true));
        assert!(!ssrf_allows("169.254.169.254", true));
        assert!(ssrf_allows("api.example.com", true));
    }

    #[test]
    fn authorities_carry_non_standard_ports() {
        let single = upstream(
            "backend",
            vec![endpoint(EndpointScheme::Http, "a.internal", Some(8080))],
        );
        let picked = select_endpoint(&single, None, 0).expect("selected");
        assert_eq!(endpoint_authority(picked), "a.internal:8080");
        assert_eq!(endpoint_base_url(picked), "http://a.internal:8080");
        let standard_upstream = upstream(
            "api",
            vec![endpoint(EndpointScheme::Https, "api.example.com", None)],
        );
        let picked = select_endpoint(&standard_upstream, None, 0).expect("selected");
        assert_eq!(endpoint_authority(picked), "api.example.com");
        assert_eq!(endpoint_base_url(picked), "https://api.example.com");
    }
}
