//! Endpoint selection within a pool.
//!
//! The behaviour matrix is `ADR/0001-request-routing.md`
//! §"X-OAGW-Target-Host Behavior Matrix": a single-endpoint upstream ignores
//! (but still validates) the header, an explicitly-aliased pool round-robins
//! unless the header pins an endpoint, and a common-suffix alias *requires*
//! the header because the alias names no single host.

use dashmap::DashMap;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{Endpoint, Upstream};

/// How the endpoint was chosen — mirrors the `selection_method` metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// Pinned by `X-OAGW-Target-Host`.
    ExplicitHeader,
    /// Chosen by round-robin across the pool.
    RoundRobin,
    /// The pool has exactly one endpoint.
    Default,
}

impl SelectionMethod {
    /// Metric label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitHeader => "explicit_header",
            Self::RoundRobin => "round_robin",
            Self::Default => "default",
        }
    }
}

/// Round-robin cursors, one per upstream.
#[derive(Debug, Default)]
pub struct EndpointSelector {
    cursors: DashMap<Uuid, AtomicUsize>,
}

impl EndpointSelector {
    /// Empty selector.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Choose the endpoint for one request.
    ///
    /// # Errors
    ///
    /// The three `400` target-host errors from `docs/DESIGN.md` §3.3, or a
    /// `503` when the pool is empty.
    pub fn select<'a>(
        &self,
        upstream: &'a Upstream,
        target_host: Option<&str>,
    ) -> DomainResult<(&'a Endpoint, SelectionMethod)> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(DomainError::link_unavailable(format!(
                "upstream '{}' has no endpoints",
                upstream.alias
            )));
        }
        let valid_hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();

        if let Some(raw) = target_host {
            let host = validate_target_host(raw, upstream, &valid_hosts)?;
            let endpoint = endpoints
                .iter()
                .find(|e| e.host.eq_ignore_ascii_case(&host))
                .ok_or_else(|| {
                    DomainError::unknown_target_host(format!(
                        "X-OAGW-Target-Host '{host}' does not match any configured endpoint. \
                         Valid hosts: [{}]",
                        valid_hosts.join(", ")
                    ))
                    .with_extension("upstream_id", upstream_gts_id(upstream))
                    .with_extension("invalid_value", host.clone())
                    .with_extension("valid_hosts", json!(valid_hosts))
                })?;
            return Ok((endpoint, SelectionMethod::ExplicitHeader));
        }

        if endpoints.len() == 1 {
            // `first()` is guaranteed by the emptiness check above.
            let endpoint = endpoints.first().ok_or_else(|| {
                DomainError::internal("endpoint pool became empty during selection")
            })?;
            return Ok((endpoint, SelectionMethod::Default));
        }

        // A common-suffix alias names no single host, so the caller must say
        // which endpoint it wants.
        if alias::compute_derived_alias(endpoints).is_some() {
            return Err(DomainError::missing_target_host(format!(
                "X-OAGW-Target-Host header required for multi-endpoint upstream with common \
                 suffix alias. Valid hosts: [{}]",
                valid_hosts.join(", ")
            ))
            .with_extension("upstream_id", upstream_gts_id(upstream))
            .with_extension("alias", upstream.alias.clone())
            .with_extension("valid_hosts", json!(valid_hosts)));
        }

        let cursor = self.cursors.entry(upstream.id).or_default();
        let index = cursor.fetch_add(1, Ordering::Relaxed) % endpoints.len();
        let endpoint = endpoints
            .get(index)
            .ok_or_else(|| DomainError::internal("round-robin index out of range"))?;
        Ok((endpoint, SelectionMethod::RoundRobin))
    }

    /// Drop the cursor for a deleted upstream.
    pub fn forget(&self, upstream_id: Uuid) {
        self.cursors.remove(&upstream_id);
    }
}

/// Anonymous GTS identifier of an upstream, for error extension members.
fn upstream_gts_id(upstream: &Upstream) -> String {
    crate::domain::gts_helpers::anonymous_id(
        crate::domain::gts_helpers::UPSTREAM_TYPE,
        upstream.id,
    )
}

/// A target host must be a bare hostname or IP: no port, path or userinfo.
fn validate_target_host(
    raw: &str,
    upstream: &Upstream,
    valid_hosts: &[String],
) -> DomainResult<String> {
    let host = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    let malformed = || {
        DomainError::invalid_target_host(
            "X-OAGW-Target-Host must be a valid hostname or IP address \
             (no port, path, or special characters)",
        )
        .with_extension("upstream_id", upstream_gts_id(upstream))
        .with_extension("invalid_value", raw.to_owned())
        .with_extension("valid_hosts", json!(valid_hosts))
    };
    if host.is_empty() || host.contains(':') || host.contains('/') || host.contains('@') {
        return Err(malformed());
    }
    if alias::validate_hostname(&host).is_err() {
        return Err(malformed());
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers;
    use crate::domain::model::{HeadersConfig, PluginsConfig, Scheme, ServerConfig};

    fn upstream(alias_value: &str, hosts: &[&str]) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias_value.to_owned(),
            enabled: true,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|h| Endpoint {
                        scheme: Scheme::Https,
                        host: (*h).to_owned(),
                        port: Some(443),
                    })
                    .collect(),
            },
            protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn single_endpoint_ignores_an_absent_header() {
        let up = upstream("api.openai.com", &["api.openai.com"]);
        let (endpoint, method) = EndpointSelector::new().select(&up, None).expect("select");
        assert_eq!(endpoint.host, "api.openai.com");
        assert_eq!(method, SelectionMethod::Default);
    }

    #[test]
    fn single_endpoint_still_validates_a_present_header() {
        let up = upstream("api.openai.com", &["api.openai.com"]);
        let selector = EndpointSelector::new();
        let (endpoint, method) = selector
            .select(&up, Some("api.openai.com"))
            .expect("matching header");
        assert_eq!(endpoint.host, "api.openai.com");
        assert_eq!(method, SelectionMethod::ExplicitHeader);

        let err = selector
            .select(&up, Some("elsewhere.example"))
            .expect_err("mismatched header");
        assert_eq!(err.status(), 400);
        assert_eq!(err.gts_type(), gts_helpers::errors::UNKNOWN_TARGET_HOST);
    }

    #[test]
    fn common_suffix_alias_requires_the_header() {
        let up = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let err = EndpointSelector::new()
            .select(&up, None)
            .expect_err("header required");
        assert_eq!(err.status(), 400);
        assert_eq!(err.gts_type(), gts_helpers::errors::MISSING_TARGET_HOST);
        assert_eq!(
            err.extensions().get("valid_hosts"),
            Some(&json!(["us.vendor.com", "eu.vendor.com"]))
        );
    }

    #[test]
    fn explicit_alias_round_robins_without_the_header() {
        let up = upstream("my-service", &["10.0.1.1", "10.0.1.2"]);
        let selector = EndpointSelector::new();
        let (first, method) = selector.select(&up, None).expect("select");
        assert_eq!(method, SelectionMethod::RoundRobin);
        let (second, _) = selector.select(&up, None).expect("select");
        assert_ne!(first.host, second.host);
        let (third, _) = selector.select(&up, None).expect("select");
        assert_eq!(first.host, third.host);
    }

    #[test]
    fn explicit_alias_honours_the_header_when_present() {
        let up = upstream("my-service", &["10.0.1.1", "10.0.1.2"]);
        let (endpoint, method) = EndpointSelector::new()
            .select(&up, Some("10.0.1.2"))
            .expect("pinned");
        assert_eq!(endpoint.host, "10.0.1.2");
        assert_eq!(method, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn a_malformed_header_is_a_distinct_error_type() {
        let up = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let selector = EndpointSelector::new();
        for bad in ["us.vendor.com:8443", "us.vendor.com/path", "user@host", ""] {
            let err = selector.select(&up, Some(bad)).expect_err("malformed");
            assert_eq!(
                err.gts_type(),
                gts_helpers::errors::INVALID_TARGET_HOST,
                "{bad} should be rejected as malformed"
            );
        }
    }

    #[test]
    fn header_matching_is_case_insensitive() {
        let up = upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let (endpoint, _) = EndpointSelector::new()
            .select(&up, Some("US.Vendor.COM"))
            .expect("case-insensitive");
        assert_eq!(endpoint.host, "us.vendor.com");
    }
}
