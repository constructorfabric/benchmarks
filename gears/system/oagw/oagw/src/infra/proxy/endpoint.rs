//! Endpoint selection, including the `X-OAGW-Target-Host` behaviour matrix
//! from ADR-0001.

use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts_helpers;
use crate::domain::model::{Endpoint, Upstream};

/// How the endpoint was picked — the `selection_method` metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    ExplicitHeader,
    RoundRobin,
    Default,
}

impl SelectionMethod {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// Round-robin cursors, one per upstream.
#[derive(Default)]
pub struct EndpointSelector {
    cursors: DashMap<Uuid, AtomicUsize>,
}

impl EndpointSelector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply the `X-OAGW-Target-Host` matrix and pick an endpoint.
    ///
    /// # Errors
    /// * `400 InvalidTargetHost` — the header is not a bare hostname or IP.
    /// * `400 UnknownTargetHost` — it names no configured endpoint.
    /// * `400 MissingTargetHost` — a multi-endpoint pool reached through a
    ///   common-suffix alias was called without the header.
    pub fn select<'a>(
        &self,
        upstream: &'a Upstream,
        target_host: Option<&str>,
    ) -> OagwResult<(&'a Endpoint, SelectionMethod)> {
        let endpoints = &upstream.server.endpoints;
        let first = endpoints.first().ok_or_else(|| {
            OagwError::internal(format!(
                "upstream '{}' has no endpoints",
                upstream.alias
            ))
        })?;

        if let Some(raw) = target_host {
            let host = validate_target_host(raw, upstream)?;
            let matched = endpoints
                .iter()
                .find(|e| e.host.eq_ignore_ascii_case(&host))
                .ok_or_else(|| unknown_target_host(&host, upstream))?;
            return Ok((matched, SelectionMethod::ExplicitHeader));
        }

        if endpoints.len() == 1 {
            return Ok((first, SelectionMethod::Default));
        }

        // A common-suffix alias does not name any single endpoint, so the
        // caller has to disambiguate.
        if alias::alias_is_common_suffix(endpoints, &upstream.alias) {
            return Err(missing_target_host(upstream));
        }

        let cursor = self
            .cursors
            .entry(upstream.id)
            .or_insert_with(|| AtomicUsize::new(0));
        let index = cursor.fetch_add(1, Ordering::Relaxed) % endpoints.len();
        let chosen = endpoints.get(index).unwrap_or(first);
        Ok((chosen, SelectionMethod::RoundRobin))
    }
}

/// Validate the header value: a bare hostname or IP, no port, path or scheme.
///
/// # Errors
/// `400 InvalidTargetHost` when the value is not a bare host.
pub fn validate_target_host(raw: &str, upstream: &Upstream) -> OagwResult<String> {
    let trimmed = raw.trim();
    let invalid = || {
        OagwError::new(
            ErrorKind::InvalidTargetHost,
            "X-OAGW-Target-Host must be a valid hostname or IP address (no port, path, or \
             special characters)",
        )
        .with_ext(
            "upstream_id",
            gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, upstream.id),
        )
        .with_ext("invalid_value", trimmed.to_owned())
    };

    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.contains(' ')
        || trimmed.contains('@')
    {
        return Err(invalid());
    }
    // A bracketed IPv6 literal is the only place a colon is legal.
    let bracketed = trimmed.starts_with('[') && trimmed.ends_with(']');
    if trimmed.contains(':') && !bracketed && trimmed.parse::<std::net::Ipv6Addr>().is_err() {
        return Err(invalid());
    }
    let host = alias::normalize_host(trimmed);
    if !alias::is_ip_literal(&host) && alias::validate_hostname(&host).is_err() {
        return Err(invalid());
    }
    Ok(host)
}

fn valid_hosts(upstream: &Upstream) -> Value {
    Value::Array(upstream.hosts().into_iter().map(Value::String).collect())
}

fn unknown_target_host(host: &str, upstream: &Upstream) -> OagwError {
    let hosts = upstream.hosts();
    OagwError::new(
        ErrorKind::UnknownTargetHost,
        format!(
            "X-OAGW-Target-Host '{host}' does not match any configured endpoint. Valid hosts: \
             [{}]",
            hosts.join(", ")
        ),
    )
    .with_ext(
        "upstream_id",
        gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, upstream.id),
    )
    .with_ext("invalid_value", host.to_owned())
    .with_ext("valid_hosts", valid_hosts(upstream))
}

fn missing_target_host(upstream: &Upstream) -> OagwError {
    let hosts = upstream.hosts();
    OagwError::new(
        ErrorKind::MissingTargetHost,
        format!(
            "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix \
             alias. Valid hosts: [{}]",
            hosts.join(", ")
        ),
    )
    .with_ext(
        "upstream_id",
        gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, upstream.id),
    )
    .with_ext("alias", upstream.alias.clone())
    .with_ext("valid_hosts", valid_hosts(upstream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        HeadersConfig, PluginsConfig, Scheme, ServerConfig,
    };

    fn upstream(alias: &str, hosts: &[&str]) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
            enabled: true,
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|h| Endpoint {
                        scheme: Scheme::Https,
                        host: (*h).to_owned(),
                        port: 443,
                    })
                    .collect(),
            },
            auth: None,
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn single_endpoint_ignores_an_absent_header() {
        let u = upstream("api.openai.com", &["api.openai.com"]);
        let (endpoint, method) = EndpointSelector::new().select(&u, None).expect("select");
        assert_eq!(endpoint.host, "api.openai.com");
        assert_eq!(method, SelectionMethod::Default);
    }

    #[test]
    fn single_endpoint_validates_a_present_header() {
        let u = upstream("api.openai.com", &["api.openai.com"]);
        let sel = EndpointSelector::new();
        assert!(sel.select(&u, Some("api.openai.com")).is_ok());
        let err = sel.select(&u, Some("other.example.com")).expect_err("unknown");
        assert_eq!(err.status(), 400);
        assert!(err.kind.gts_type().ends_with("unknown_target_host.v1"));
    }

    #[test]
    fn multi_endpoint_with_explicit_alias_round_robins() {
        let u = upstream("my-service", &["server-a.example.com", "server-b.example.com"]);
        let sel = EndpointSelector::new();
        let (first, method) = sel.select(&u, None).expect("first");
        assert_eq!(method, SelectionMethod::RoundRobin);
        let (second, _) = sel.select(&u, None).expect("second");
        assert_ne!(first.host, second.host);
        let (third, _) = sel.select(&u, None).expect("third");
        assert_eq!(first.host, third.host);
    }

    #[test]
    fn multi_endpoint_with_explicit_alias_honours_the_header() {
        let u = upstream("my-service", &["server-a.example.com", "server-b.example.com"]);
        let (endpoint, method) = EndpointSelector::new()
            .select(&u, Some("server-b.example.com"))
            .expect("select");
        assert_eq!(endpoint.host, "server-b.example.com");
        assert_eq!(method, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn multi_endpoint_with_common_suffix_alias_requires_the_header() {
        let u = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let sel = EndpointSelector::new();
        let err = sel.select(&u, None).expect_err("missing header");
        assert_eq!(err.status(), 400);
        assert!(err.kind.gts_type().ends_with("missing_target_host.v1"));
        assert_eq!(
            err.extensions.get("alias").and_then(Value::as_str),
            Some("vendor.com")
        );
        assert!(err.extensions.contains_key("valid_hosts"));

        let (endpoint, _) = sel.select(&u, Some("eu.vendor.com")).expect("with header");
        assert_eq!(endpoint.host, "eu.vendor.com");
    }

    #[test]
    fn malformed_header_values_are_rejected() {
        let u = upstream("my-service", &["server-a.example.com", "server-b.example.com"]);
        let sel = EndpointSelector::new();
        for bad in [
            "us.vendor.com:8443",
            "https://us.vendor.com",
            "us.vendor.com/path",
            "us vendor",
            "",
        ] {
            let err = sel.select(&u, Some(bad)).expect_err(bad);
            assert_eq!(err.status(), 400, "{bad}");
            assert!(
                err.kind.gts_type().ends_with("invalid_target_host.v1"),
                "{bad}: {}",
                err.kind.gts_type()
            );
        }
    }

    #[test]
    fn header_matching_is_case_insensitive() {
        let u = upstream("my-service", &["server-a.example.com", "server-b.example.com"]);
        let (endpoint, _) = EndpointSelector::new()
            .select(&u, Some("Server-A.Example.COM"))
            .expect("select");
        assert_eq!(endpoint.host, "server-a.example.com");
    }
}
