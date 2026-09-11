//! Multi-endpoint pool selection and target-host validation.
//!
//! `cpt-cf-oagw-algo-endpoint-selection` picks the one endpoint of the pool the
//! request dials. A single-endpoint pool is selected directly; a multi-endpoint
//! pool is balanced by a per-upstream round-robin cursor that is advanced, never
//! re-scanned; and a pool whose alias is the PSL-derived common suffix of its
//! hosts requires the caller to name the endpoint it wants through
//! `X-OAGW-Target-Host`.
//!
//! The header is a routing header only: it is read once here, recorded on the
//! request context and stripped from the outbound header set, so it is never
//! forwarded to the upstream.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::alias::{self, AliasDerivation};
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, ServerConfig, Upstream};
use crate::domain::validation::is_valid_hostname;
use crate::infra::proxy::context::SelectionMethod;

/// The routing header that names the endpoint of a pool to dial.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// The endpoint selected for one request.
#[derive(Debug, Clone)]
pub struct Selection {
    /// The endpoint the call connects to.
    pub endpoint: Endpoint,
    /// How the endpoint was selected, for entry 2.7's routing metric.
    pub method: SelectionMethod,
    /// The value the caller named, when it named one.
    pub requested_host: Option<String>,
}

/// The per-upstream round-robin cursors.
///
/// One cursor per upstream, advanced by one on every balanced request: the
/// cursor is never re-scanned, so a pool that grows or shrinks keeps serving
/// from where it left off.
#[derive(Default)]
pub struct RoundRobin {
    cursors: DashMap<Uuid, Arc<AtomicU64>>,
}

impl RoundRobin {
    /// The next index of the pool for `upstream_id`.
    fn next(&self, upstream_id: Uuid, len: usize) -> usize {
        let cursor = self
            .cursors
            .entry(upstream_id)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        let next = cursor.fetch_add(1, Ordering::Relaxed);
        usize::try_from(next.checked_rem(u64::try_from(len).unwrap_or(1)).unwrap_or(0))
            .unwrap_or(0)
    }

    /// The cursor position recorded for `upstream_id`, for the tests.
    #[must_use]
    pub fn cursor(&self, upstream_id: Uuid) -> u64 {
        self.cursors
            .get(&upstream_id)
            .map(|cursor| cursor.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

/// Normalize a caller-named target host.
///
/// The value must be a hostname or an IP literal with no port, no path and no
/// special characters. Returns the normalized comparison form, or `None` when
/// the value is malformed.
#[must_use]
pub fn parse_target_host(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 253 {
        return None;
    }
    if trimmed.chars().any(char::is_control) {
        return None;
    }
    // An IPv6 literal may arrive bracketed, as an authority is written in a
    // URL, or bare as the endpoint stores it; either way the address inside is
    // the target and nothing else may carry a colon.
    let literal = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Ok(address) = literal.parse::<std::net::IpAddr>() {
        return Some(address.to_string());
    }
    // A routing header carries a bare authority: no scheme, no port, no path
    // and none of the characters that would smuggle one in.
    if trimmed
        .chars()
        .any(|c| matches!(c, '/' | ':' | '@' | '?' | '#' | '%' | '\\' | ' ' | '\t' | ','))
    {
        return None;
    }
    let lowered = trimmed.to_ascii_lowercase();
    let lowered = lowered.strip_suffix('.').unwrap_or(&lowered).to_owned();
    if lowered.is_empty() {
        return None;
    }
    is_valid_hostname(&lowered).then_some(lowered)
}

/// The hosts of the pool, normalized for comparison.
fn pool_hosts(pool: &[Endpoint]) -> Vec<String> {
    pool.iter()
        .map(|endpoint| {
            endpoint
                .host
                .trim()
                .to_ascii_lowercase()
                .trim_end_matches('.')
                .to_owned()
        })
        .collect()
}

/// Whether the upstream's alias is the PSL-validated common suffix of the
/// pool's hosts, i.e. a hostname-derived alias rather than a configured one.
fn alias_names_the_pool(upstream: &Upstream) -> bool {
    matches!(
        alias::derive(&upstream.server),
        AliasDerivation::Derived { alias } if alias == upstream.alias
    )
}

// @cpt-begin:cpt-cf-oagw-dod-endpoint-pool:p1:inst-full
/// Select the endpoint of `upstream`'s pool for one request.
///
/// # Errors
///
/// Returns the mapped `400` of a missing, malformed or unknown
/// `X-OAGW-Target-Host`, carrying the ADR 0007 extension fields.
pub fn select(
    upstream: &Upstream,
    target_host: Option<&str>,
    cursors: &RoundRobin,
) -> Result<Selection, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-01
    // The header is read once and treated as a routing header only; the value
    // never reaches the outbound header set.
    let pool: Vec<Endpoint> = upstream.server.pool();
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-01
    let Some(first) = pool.first() else {
        return Err(DomainError::LinkUnavailable {
            detail: format!("the pool of `{}` holds no endpoint", upstream.alias),
            retry_after_seconds: None,
        });
    };

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-02
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-03
    // A single-endpoint pool has nothing to balance: the endpoint is selected
    // directly, the header is optional, and a value that is present is still
    // validated so a typo is reported rather than silently ignored.
    if pool.len() == 1 {
        if let Some(value) = target_host {
            validate_against_pool(&pool, Some(value), upstream)?;
        }
        return Ok(Selection {
            endpoint: first.clone(),
            method: SelectionMethod::Default,
            requested_host: target_host.map(str::to_owned),
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-03
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-02

    let (index, method, requested) = if let Some(value) = target_host {
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-04
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-05
        // A malformed value is refused before it is compared against the pool,
        // so a caller learns that its value is not an address rather than that
        // the pool does not contain it.
        let Some(named) = parse_target_host(value) else {
            return Err(DomainError::InvalidTargetHost {
                detail: format!(
                    "`{TARGET_HOST_HEADER}` must be a hostname or an IP literal with no port"
                ),
            });
        };
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-05

        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-06
        // The comparison is case-insensitive over the trailing dot, so
        // `A.Vendor.Com.` names the endpoint stored as `a.vendor.com`.
        let hosts = pool_hosts(&pool);
        let Some(index) = hosts.iter().position(|host| *host == named) else {
            return Err(DomainError::UnknownTargetHost {
                detail: format!(
                    "`{TARGET_HOST_HEADER}` does not name an endpoint of `{}`",
                    upstream.alias
                ),
            });
        };
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-06

        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-07
        // The named endpoint bypasses the cursor: the caller asked for a
        // specific endpoint and the balancer must not move it.
        (index, SelectionMethod::ExplicitHeader, Some(value.to_owned()))
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-07
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-04
    } else {
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-08
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-09
        // A hostname-derived alias is the pool's common suffix: it names the
        // pool and not any endpoint of it, so balancing would pick an endpoint
        // the caller did not ask for, and the caller must name one.
        if alias_names_the_pool(upstream) {
            // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-10
            return Err(DomainError::MissingTargetHost {
                detail: format!(
                    "the pool of `{}` holds {} endpoints and `{TARGET_HOST_HEADER}` is required",
                    upstream.alias,
                    pool.len()
                ),
            });
            // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-10
        }
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-09

        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-11
        // The cursor advances by one and is taken modulo the pool length: no
        // scan of the pool happens, so a request never walks it from the start.
        let index = cursors.next(upstream.id, pool.len());
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-11
        (index, SelectionMethod::RoundRobin, None)
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-08
    };

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-12
    // The selection method is recorded for entry 2.7's routing metric.
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-14
    // The selected endpoint is the return value: the pool slice, the cursor and
    // the header value are consumed here and nothing of the selection but the
    // endpoint and its method leaves this stage.
    Ok(Selection {
        endpoint: pool[index].clone(),
        method,
        requested_host: requested,
    })
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-14
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-pe-es-12
}
// @cpt-end:cpt-cf-oagw-dod-endpoint-pool:p1:inst-full

/// Validate a present-but-optional header value against a single-endpoint pool.
fn validate_against_pool(
    pool: &[Endpoint],
    value: Option<&str>,
    upstream: &Upstream,
) -> Result<(), DomainError> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(named) = parse_target_host(value) else {
        return Err(DomainError::InvalidTargetHost {
            detail: format!("`{TARGET_HOST_HEADER}` must be a hostname or an IP literal"),
        });
    };
    let hosts = pool_hosts(pool);
    if !hosts.contains(&named) {
        return Err(DomainError::UnknownTargetHost {
            detail: format!(
                "`{TARGET_HOST_HEADER}` does not name an endpoint of `{}`",
                upstream.alias
            ),
        });
    }
    Ok(())
}

/// The hosts the caller may name, in pool order, for the ADR 0007
/// `valid_hosts` extension field.
#[must_use]
pub fn valid_hosts(server: &ServerConfig) -> Vec<String> {
    server.pool().iter().map(|endpoint| endpoint.host.clone()).collect()
}

/// Whether a route is addressable without naming an endpoint.
///
/// A single-endpoint pool needs no header, and a multi-endpoint pool whose
/// alias is an explicit name balances on the cursor.
#[must_use]
pub fn requires_target_host(upstream: &Upstream) -> bool {
    upstream.server.pool().len() > 1 && alias_names_the_pool(upstream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Protocol, Timestamp};

    fn upstream(id: Uuid, alias: &str, hosts: &[&str]) -> Upstream {
        Upstream {
            id,
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: alias.to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|host| Endpoint {
                        scheme: crate::domain::model::Scheme::Https,
                        host: (*host).to_owned(),
                        port: 443,
                    })
                    .collect(),
            },
            protocol: Protocol::Http,
            auth: None,
            auth_plugin_ref: None,
            auth_plugin_uuid: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            created_at: Timestamp::now(),
        }
    }

    #[test]
    fn a_single_endpoint_pool_is_selected_directly() {
        let record = upstream(Uuid::new_v4(), "api.vendor.com", &["api.vendor.com"]);
        let cursors = RoundRobin::default();
        let selection = select(&record, None, &cursors).expect("the pool has one endpoint");
        assert_eq!(selection.endpoint.host, "api.vendor.com");
        assert_eq!(selection.method, SelectionMethod::Default);
        assert!(selection.requested_host.is_none());
    }

    #[test]
    fn a_present_header_is_still_validated_on_a_single_endpoint_pool() {
        let record = upstream(Uuid::new_v4(), "api.vendor.com", &["api.vendor.com"]);
        let error = select(&record, Some("other.vendor.com"), &RoundRobin::default())
            .expect_err("the value names no endpoint of the pool");
        assert_eq!(error.status(), 400, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
        );
    }

    #[test]
    fn a_malformed_target_host_is_an_invalid_target_host() {
        let record = upstream(Uuid::new_v4(), "vendor.com", &["a.vendor.com", "b.vendor.com"]);
        for value in ["a.vendor.com:443", "a.vendor.com/x", "a vendor.com", "[::1]:8", ""] {
            let error = select(&record, Some(value), &RoundRobin::default())
                .expect_err("the value is malformed");
            assert_eq!(error.status(), 400, "{error}");
            assert_eq!(
                error.gts_id(),
                "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
                "{value}"
            );
        }
    }

    #[test]
    fn a_target_host_is_compared_case_insensitively_without_the_trailing_dot() {
        let record = upstream(Uuid::new_v4(), "vendor.com", &["a.vendor.com", "b.vendor.com"]);
        let selection = select(&record, Some("B.Vendor.Com."), &RoundRobin::default())
            .expect("the value names an endpoint");
        assert_eq!(selection.endpoint.host, "b.vendor.com");
        assert_eq!(selection.method, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn an_unknown_target_host_names_no_endpoint_of_the_pool() {
        let record = upstream(Uuid::new_v4(), "vendor.com", &["a.vendor.com", "b.vendor.com"]);
        let error = select(&record, Some("c.vendor.com"), &RoundRobin::default())
            .expect_err("the value names no endpoint");
        assert_eq!(error.status(), 400, "{error}");
    }

    #[test]
    fn a_hostname_derived_alias_requires_the_header() {
        let record = upstream(Uuid::new_v4(), "vendor.com", &["a.vendor.com", "b.vendor.com"]);
        assert!(requires_target_host(&record));
        let error = select(&record, None, &RoundRobin::default())
            .expect_err("the caller must name the endpoint");
        assert_eq!(error.status(), 400, "{error}");
        assert_eq!(
            error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
    }

    #[test]
    fn an_explicit_alias_balances_with_the_cursor() {
        let id = Uuid::new_v4();
        let record = upstream(id, "payment-edge", &["a.vendor.com", "b.vendor.com"]);
        assert!(!requires_target_host(&record));
        let cursors = RoundRobin::default();
        let first = select(&record, None, &cursors).expect("the pool balances");
        let second = select(&record, None, &cursors).expect("the pool balances");
        let third = select(&record, None, &cursors).expect("the pool balances");
        assert_eq!(first.method, SelectionMethod::RoundRobin);
        assert_ne!(first.endpoint.host, second.endpoint.host);
        assert_eq!(third.endpoint.host, first.endpoint.host);
        assert_eq!(cursors.cursor(id), 3);
    }

    #[test]
    fn an_explicit_header_bypasses_the_cursor() {
        let id = Uuid::new_v4();
        let record = upstream(id, "payment-edge", &["a.vendor.com", "b.vendor.com"]);
        let cursors = RoundRobin::default();
        for _ in 0..3 {
            let selection = select(&record, Some("b.vendor.com"), &cursors)
                .expect("the value names an endpoint");
            assert_eq!(selection.endpoint.host, "b.vendor.com");
        }
        assert_eq!(cursors.cursor(id), 0, "the cursor is not advanced");
    }

    #[test]
    fn an_ip_literal_is_a_valid_target_host_value() {
        assert_eq!(
            parse_target_host("10.0.0.7").as_deref(),
            Some("10.0.0.7")
        );
        assert_eq!(parse_target_host("::1").as_deref(), Some("::1"));
        assert_eq!(parse_target_host("[2001:db8::1]").as_deref(), Some("2001:db8::1"));
        assert_eq!(parse_target_host("A.Vendor.COM.").as_deref(), Some("a.vendor.com"));
    }

    #[test]
    fn a_target_host_value_is_never_a_route_or_a_port() {
        assert!(parse_target_host("").is_none());
        assert!(parse_target_host("host:443").is_none());
        assert!(parse_target_host("host/path").is_none());
        assert!(parse_target_host("host?q=1").is_none());
        assert!(parse_target_host("host#f").is_none());
        assert!(parse_target_host("user@host").is_none());
        assert!(parse_target_host("host%00").is_none());
    }

    #[test]
    fn the_valid_hosts_are_the_pool_in_declaration_order() {
        let record = upstream(Uuid::new_v4(), "vendor.com", &["b.vendor.com", "a.vendor.com"]);
        assert_eq!(
            valid_hosts(&record.server),
            vec!["b.vendor.com".to_owned(), "a.vendor.com".to_owned()]
        );
    }

    #[test]
    fn the_pool_of_a_mixed_declaration_is_the_matching_slice() {
        // The entry-2.2 pool rule: the pool holds the endpoints that share the
        // first endpoint's protocol kind and port.
        let mut record = upstream(Uuid::new_v4(), "vendor.com", &["a.vendor.com"]);
        record.server.endpoints.push(Endpoint {
            scheme: crate::domain::model::Scheme::Https,
            host: "b.vendor.com".to_owned(),
            port: 8443,
        });
        assert_eq!(record.server.pool().len(), 1);
    }
}
