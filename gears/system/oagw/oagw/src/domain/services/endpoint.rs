//! Endpoint selection within an upstream's load-balance pool, including the
//! `X-OAGW-Target-Host` behaviour matrix from ADR-0001 Appendix A.

use serde_json::Value;

use crate::domain::alias;
use crate::domain::error::OagwError;
use crate::domain::model::{Endpoint, Upstream};

/// Header naming a specific endpoint in a multi-endpoint pool. Consumed
/// during routing and never forwarded upstream.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// How the endpoint was chosen — mirrored into the routing metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// Named by `X-OAGW-Target-Host`.
    ExplicitHeader,
    /// Distributed across the pool.
    RoundRobin,
    /// The pool has exactly one endpoint.
    Default,
}

impl SelectionMethod {
    /// Metric label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SelectionMethod::ExplicitHeader => "explicit_header",
            SelectionMethod::RoundRobin => "round_robin",
            SelectionMethod::Default => "default",
        }
    }
}

/// `true` when the upstream's alias was derived from a multi-host common
/// suffix — the one case in which `X-OAGW-Target-Host` is mandatory, because
/// the alias names the shared domain rather than any single endpoint.
#[must_use]
pub fn alias_is_common_suffix(upstream: &Upstream) -> bool {
    let endpoints = &upstream.spec.server.endpoints;
    endpoints.len() > 1
        && alias::compute_derived_alias(endpoints).as_deref() == Some(upstream.alias())
}

/// Validate the raw `X-OAGW-Target-Host` value: a bare hostname or IP
/// literal, with no port, path, scheme or userinfo.
///
/// # Errors
///
/// Returns `400 InvalidTargetHost` for anything else.
pub fn validate_target_host(raw: &str) -> Result<String, OagwError> {
    let value = raw.trim();
    let invalid = || {
        OagwError::invalid_target_host(
            "X-OAGW-Target-Host must be a valid hostname or IP address (no port, path, or \
             special characters)",
        )
        .with("invalid_value", value.to_owned())
    };
    if value.is_empty()
        || value.contains(':') && !alias::is_ip_literal(value)
        || value.contains('/')
        || value.contains('@')
        || value.contains('?')
        || value.contains('#')
        || value.contains(' ')
    {
        return Err(invalid());
    }
    let normalized = alias::normalize_host(value);
    if alias::is_ip_literal(&normalized) {
        return Ok(normalized);
    }
    alias::validate_hostname(&normalized).map_err(|_| invalid())?;
    Ok(normalized)
}

/// Choose the endpoint a request should be sent to.
///
/// `next_index` supplies the round-robin cursor; it is only consulted when
/// the pool has more than one endpoint and no header was supplied.
///
/// # Errors
///
/// * `400 MissingTargetHost` — multi-endpoint pool behind a common-suffix
///   alias with no header;
/// * `400 InvalidTargetHost` — malformed header;
/// * `400 UnknownTargetHost` — header names no configured endpoint.
pub fn select_endpoint(
    upstream: &Upstream,
    target_host: Option<&str>,
    next_index: usize,
) -> Result<(Endpoint, SelectionMethod), OagwError> {
    let endpoints = &upstream.spec.server.endpoints;
    if endpoints.is_empty() {
        return Err(OagwError::link_unavailable(
            "upstream has no configured endpoints",
        ));
    }
    let valid_hosts: Vec<Value> = endpoints
        .iter()
        .map(|e| Value::from(e.host.clone()))
        .collect();

    if let Some(raw) = target_host {
        let host = validate_target_host(raw).map_err(|err| {
            err.with("upstream_id", upstream_gts_id(upstream))
                .with("alias", upstream.alias().to_owned())
        })?;
        let hit = endpoints
            .iter()
            .find(|e| e.host.eq_ignore_ascii_case(&host))
            .cloned();
        return match hit {
            Some(endpoint) => Ok((endpoint, SelectionMethod::ExplicitHeader)),
            None => Err(OagwError::unknown_target_host(format!(
                "X-OAGW-Target-Host '{host}' does not match any configured endpoint. Valid \
                 hosts: [{}]",
                host_list(endpoints)
            ))
            .with("upstream_id", upstream_gts_id(upstream))
            .with("alias", upstream.alias().to_owned())
            .with("invalid_value", host)
            .with("valid_hosts", Value::Array(valid_hosts))),
        };
    }

    if endpoints.len() == 1 {
        return Ok((endpoints[0].clone(), SelectionMethod::Default));
    }

    if alias_is_common_suffix(upstream) {
        return Err(OagwError::missing_target_host(format!(
            "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix \
             alias. Valid hosts: [{}]",
            host_list(endpoints)
        ))
        .with("upstream_id", upstream_gts_id(upstream))
        .with("alias", upstream.alias().to_owned())
        .with("valid_hosts", Value::Array(valid_hosts)));
    }

    let endpoint = endpoints[next_index % endpoints.len()].clone();
    Ok((endpoint, SelectionMethod::RoundRobin))
}

fn host_list(endpoints: &[Endpoint]) -> String {
    endpoints
        .iter()
        .map(|e| e.host.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

fn upstream_gts_id(upstream: &Upstream) -> String {
    crate::domain::gts_helpers::anonymous_id(crate::domain::gts_helpers::UPSTREAM_TYPE, upstream.id)
}

#[cfg(test)]
mod tests {
    use super::{SelectionMethod, alias_is_common_suffix, select_endpoint, validate_target_host};
    use crate::domain::gts_helpers as gts;
    use crate::domain::model::{Endpoint, ServerConfig, Upstream, UpstreamSpec};
    use crate::domain::timeutil;
    use uuid::Uuid;

    fn upstream(alias: &str, hosts: &[&str]) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            created_at: timeutil::now_rfc3339(),
            updated_at: timeutil::now_rfc3339(),
            spec: UpstreamSpec {
                enabled: true,
                alias: alias.to_owned(),
                tags: Vec::new(),
                server: ServerConfig {
                    endpoints: hosts
                        .iter()
                        .map(|h| Endpoint {
                            scheme: "https".to_owned(),
                            host: (*h).to_owned(),
                            port: 443,
                        })
                        .collect(),
                },
                protocol: gts::PROTOCOL_HTTP.to_owned(),
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        }
    }

    #[test]
    fn single_endpoint_needs_no_header() {
        let u = upstream("api.openai.com", &["api.openai.com"]);
        let (endpoint, how) = select_endpoint(&u, None, 0).expect("selected");
        assert_eq!(endpoint.host, "api.openai.com");
        assert_eq!(how, SelectionMethod::Default);
    }

    #[test]
    fn single_endpoint_validates_a_supplied_header() {
        let u = upstream("api.openai.com", &["api.openai.com"]);
        let (_, how) = select_endpoint(&u, Some("api.openai.com"), 0).expect("selected");
        assert_eq!(how, SelectionMethod::ExplicitHeader);
        let err = select_endpoint(&u, Some("evil.example.com"), 0).expect_err("unknown");
        assert_eq!(err.status, 400);
        assert_eq!(err.error_type, gts::ERR_UNKNOWN_TARGET_HOST);
    }

    #[test]
    fn common_suffix_alias_requires_the_header() {
        let u = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        assert!(alias_is_common_suffix(&u));
        let err = select_endpoint(&u, None, 0).expect_err("header required");
        assert_eq!(err.error_type, gts::ERR_MISSING_TARGET_HOST);
        assert_eq!(err.status, 400);
        let valid = err.extensions.get("valid_hosts").expect("hosts listed");
        assert_eq!(valid.as_array().map(Vec::len), Some(2));

        let (endpoint, how) = select_endpoint(&u, Some("eu.vendor.com"), 0).expect("selected");
        assert_eq!(endpoint.host, "eu.vendor.com");
        assert_eq!(how, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn explicit_alias_pool_round_robins() {
        let u = upstream(
            "my-service",
            &["server-a.example.com", "server-b.example.com"],
        );
        assert!(!alias_is_common_suffix(&u));
        let (first, how) = select_endpoint(&u, None, 0).expect("selected");
        assert_eq!(how, SelectionMethod::RoundRobin);
        let (second, _) = select_endpoint(&u, None, 1).expect("selected");
        assert_ne!(first.host, second.host);
        let (third, _) = select_endpoint(&u, None, 2).expect("wraps");
        assert_eq!(first.host, third.host);
    }

    #[test]
    fn target_host_format_is_strict() {
        assert_eq!(
            validate_target_host("US.Vendor.COM").expect("normalized"),
            "us.vendor.com"
        );
        validate_target_host("10.0.1.1").expect("ipv4 literal");
        for bad in [
            "us.vendor.com:8443",
            "https://us.vendor.com",
            "us.vendor.com/path",
            "user@us.vendor.com",
            "us.vendor.com?x=1",
            "",
            "has space",
        ] {
            let err = validate_target_host(bad).expect_err("rejected");
            assert_eq!(err.error_type, gts::ERR_INVALID_TARGET_HOST, "{bad}");
        }
    }
}
