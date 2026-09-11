//! Select the Target Endpoint from the Pool
//! (`cpt-cf-oagw-algo-proxy-select-endpoint`).

use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use uuid::Uuid;

use crate::model::upstream::alias::{AliasClass, classify_pool};
use crate::model::upstream::{Endpoint, EndpointScheme, Upstream};

/// How the returned endpoint was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SelectionMethod {
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

#[derive(Debug, Clone)]
pub(crate) struct SelectedEndpoint {
    pub endpoint: Endpoint,
    pub method: SelectionMethod,
}

/// The three `400` target-host failures, each carrying the extension-field
/// values its documented body specifies.
#[derive(Debug, Clone)]
pub(crate) enum TargetHostError {
    Missing {
        valid_hosts: Vec<String>,
    },
    Invalid {
        invalid_value: String,
    },
    Unknown {
        invalid_value: String,
        valid_hosts: Vec<String>,
    },
}

/// Per-upstream in-process round-robin cursor state
/// (`inst-proxy-ep-cursor-state`): not persisted, not shared between
/// instances, resets to the first endpoint on restart.
#[derive(Debug, Default)]
pub(crate) struct RoundRobinState {
    cursors: DashMap<Uuid, AtomicUsize>,
}

impl RoundRobinState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-cursor-state
    fn next_index(&self, upstream_id: Uuid, pool_len: usize) -> usize {
        let cursor = self
            .cursors
            .entry(upstream_id)
            .or_insert_with(|| AtomicUsize::new(0));
        let idx = cursor.fetch_add(1, Ordering::Relaxed);
        idx % pool_len
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-cursor-state
}

fn is_standard_port(scheme: EndpointScheme, port: u16) -> bool {
    match scheme {
        EndpointScheme::Http | EndpointScheme::Ws => port == 80,
        EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc => {
            port == 443
        }
    }
}

fn normalize_header_host(raw: &str) -> Option<String> {
    let trimmed = raw.strip_suffix('.').unwrap_or(raw);
    Some(trimmed.to_ascii_lowercase())
}

/// Reject a value carrying a port, path, scheme, userinfo, whitespace or
/// control characters; accept a bare RFC 1123 hostname or IP literal
/// (`inst-proxy-ep-validate-format`).
fn validate_target_host_format(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 253 {
        return false;
    }
    if raw
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return false;
    }
    if raw.contains(':')
        || raw.contains('/')
        || raw.contains('@')
        || raw.contains('?')
        || raw.contains('#')
    {
        return false;
    }
    if raw.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    crate::model::upstream::host::is_valid_hostname(raw)
}

/// Classify the alias's derivation the same way the management layer does
/// at write time (`inst-proxy-ep-classify-alias`): a multi-endpoint pool is
/// common-suffix-derived only when `classify_pool` derives exactly the
/// stored alias.
fn is_common_suffix_derived(upstream: &Upstream) -> bool {
    let Some(alias) = &upstream.alias else {
        return false;
    };
    matches!(
        classify_pool(&upstream.server.endpoints),
        AliasClass::Derivable(derived) if &derived == alias
    )
}

/// `cpt-cf-oagw-algo-proxy-select-endpoint`: apply the full
/// `X-OAGW-Target-Host` behaviour matrix.
// @cpt-algo:cpt-cf-oagw-algo-proxy-select-endpoint:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-target-host:p1
// @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-classify-alias
// @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-pool-invariant
pub(crate) fn select_endpoint(
    upstream: &Upstream,
    target_host_header: Option<&str>,
    round_robin: &RoundRobinState,
) -> Result<SelectedEndpoint, TargetHostError> {
    let endpoints = &upstream.server.endpoints;
    let valid_hosts = || endpoints.iter().map(|e| e.host.clone()).collect::<Vec<_>>();
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-pool-invariant
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-classify-alias

    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-if-header
    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-validate-format
    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-always-validate
    if let Some(raw) = target_host_header {
        // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-cursor-no-advance
        if !validate_target_host_format(raw) {
            return Err(TargetHostError::Invalid {
                invalid_value: raw.to_owned(),
            });
        }
        // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-cursor-no-advance
        // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-always-validate
        // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-validate-format
        // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-match-endpoint
        let normalized = normalize_header_host(raw).unwrap_or_default();
        let matched = endpoints
            .iter()
            .find(|e| normalize_header_host(&e.host).as_deref() == Some(normalized.as_str()));
        // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-match-endpoint
        // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-return-explicit
        return match matched {
            Some(endpoint) => Ok(SelectedEndpoint {
                endpoint: endpoint.clone(),
                method: SelectionMethod::ExplicitHeader,
            }),
            None => Err(TargetHostError::Unknown {
                invalid_value: raw.to_owned(),
                valid_hosts: valid_hosts(),
            }),
        };
        // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-return-explicit
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-if-header

    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-if-single
    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-return-single
    if endpoints.len() == 1 {
        return Ok(SelectedEndpoint {
            endpoint: endpoints[0].clone(),
            method: SelectionMethod::Default,
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-return-single
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-if-single

    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-if-suffix
    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-return-missing
    if is_common_suffix_derived(upstream) {
        return Err(TargetHostError::Missing {
            valid_hosts: valid_hosts(),
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-return-missing
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-if-suffix

    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-else-rr
    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-return-rr
    let upstream_id = upstream.id.unwrap_or_default();
    let idx = round_robin.next_index(upstream_id, endpoints.len());
    Ok(SelectedEndpoint {
        endpoint: endpoints[idx].clone(),
        method: SelectionMethod::RoundRobin,
    })
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-return-rr
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-else-rr
}

/// The selected endpoint's authority for `Host`/`:authority` replacement:
/// `host`, plus `:port` when the port is not the scheme default
/// (`inst-proxy-hdr-authority`).
pub(crate) fn endpoint_authority(endpoint: &Endpoint) -> String {
    if is_standard_port(endpoint.scheme, endpoint.port) {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

/// `true` when `scheme` is a plaintext (non-TLS) scheme
/// (`inst-proxy-fw-if-plaintext`).
pub(crate) fn is_plaintext_scheme(scheme: EndpointScheme) -> bool {
    matches!(scheme, EndpointScheme::Http | EndpointScheme::Ws)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::upstream::ServerConfig;

    fn upstream(endpoints: Vec<Endpoint>, alias: &str) -> Upstream {
        Upstream {
            id: Some(Uuid::new_v4()),
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: Vec::new(),
            server: ServerConfig { endpoints },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id: Uuid::new_v4(),
        }
    }

    fn ep(host: &str) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port: 443,
        }
    }

    #[test]
    fn single_endpoint_is_default_without_header() {
        let up = upstream(vec![ep("a.example.com")], "a.example.com");
        let rr = RoundRobinState::new();
        let sel = select_endpoint(&up, None, &rr).unwrap();
        assert_eq!(sel.method, SelectionMethod::Default);
        assert_eq!(sel.endpoint.host, "a.example.com");
    }

    #[test]
    fn single_endpoint_with_matching_header_still_that_endpoint() {
        let up = upstream(vec![ep("a.example.com")], "a.example.com");
        let rr = RoundRobinState::new();
        let sel = select_endpoint(&up, Some("a.example.com"), &rr).unwrap();
        assert_eq!(sel.method, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn single_endpoint_with_unknown_header_is_rejected_not_ignored() {
        let up = upstream(vec![ep("a.example.com")], "a.example.com");
        let rr = RoundRobinState::new();
        let err = select_endpoint(&up, Some("b.example.com"), &rr).unwrap_err();
        assert!(matches!(err, TargetHostError::Unknown { .. }));
    }

    #[test]
    fn header_with_port_is_invalid() {
        let up = upstream(vec![ep("a.example.com")], "a.example.com");
        let rr = RoundRobinState::new();
        let err = select_endpoint(&up, Some("a.example.com:8443"), &rr).unwrap_err();
        assert!(matches!(err, TargetHostError::Invalid { .. }));
    }

    #[test]
    fn multi_endpoint_common_suffix_alias_requires_header() {
        let up = upstream(vec![ep("us.vendor.com"), ep("eu.vendor.com")], "vendor.com");
        let rr = RoundRobinState::new();
        let err = select_endpoint(&up, None, &rr).unwrap_err();
        assert!(matches!(err, TargetHostError::Missing { .. }));
    }

    #[test]
    fn multi_endpoint_explicit_alias_round_robins_in_declaration_order() {
        let up = upstream(vec![ep("a.internal"), ep("b.internal")], "my-service");
        let rr = RoundRobinState::new();
        let first = select_endpoint(&up, None, &rr).unwrap();
        let second = select_endpoint(&up, None, &rr).unwrap();
        let third = select_endpoint(&up, None, &rr).unwrap();
        assert_eq!(first.endpoint.host, "a.internal");
        assert_eq!(second.endpoint.host, "b.internal");
        assert_eq!(third.endpoint.host, "a.internal");
        assert_eq!(first.method, SelectionMethod::RoundRobin);
    }

    #[test]
    fn rejected_selection_does_not_advance_round_robin_cursor() {
        let up = upstream(vec![ep("a.internal"), ep("b.internal")], "my-service");
        let rr = RoundRobinState::new();
        let _ = select_endpoint(&up, Some("bad host"), &rr);
        let first = select_endpoint(&up, None, &rr).unwrap();
        assert_eq!(first.endpoint.host, "a.internal");
    }

    #[test]
    fn endpoint_authority_omits_standard_port() {
        assert_eq!(endpoint_authority(&ep("a.example.com")), "a.example.com");
    }

    #[test]
    fn endpoint_authority_includes_nonstandard_port() {
        let mut e = ep("a.example.com");
        e.port = 8443;
        assert_eq!(endpoint_authority(&e), "a.example.com:8443");
    }
}
