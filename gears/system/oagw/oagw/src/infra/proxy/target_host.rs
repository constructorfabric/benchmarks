//! `X-OAGW-Target-Host` selection (ADR-0001).
//!
//! The header is optional for a single-endpoint upstream and for a
//! multi-endpoint pool whose alias names the pool as a whole (round-robin
//! fills it in), and required for a multi-endpoint pool with a common-suffix
//! alias — one derived from a suffix *narrower* than any single endpoint's own
//! host, so the alias alone cannot say which member to reach.

use crate::domain::alias::is_ip_literal;
use crate::domain::dto::{Endpoint, Upstream};
use crate::domain::error::DomainError;
use crate::infra::proxy::headers::TARGET_HOST_HEADER;

/// Reads and strips `X-OAGW-Target-Host` from the inbound headers.
///
/// Routing headers are consumed and not forwarded, so the value is removed
/// before the caller builds the outbound set.
#[must_use]
pub fn take_target_host(inbound: &mut http::HeaderMap) -> Option<String> {
    let value = inbound.get(TARGET_HOST_HEADER)?.to_str().ok()?.to_owned();
    inbound.remove(TARGET_HOST_HEADER);
    Some(value)
}


/// Selects the endpoint a request should reach.
///
/// # Errors
/// Returns [`DomainError::MissingTargetHost`], [`InvalidTargetHost`] or
/// [`UnknownTargetHost`] per the behaviour matrix.
pub fn select_endpoint<'a>(
    upstream: &'a Upstream,
    requested: Option<&str>,
    round_robin: &mut u64,
) -> Result<&'a Endpoint, DomainError> {
    let endpoints = &upstream.server.endpoints;
    let Some(requested) = requested else {
        if endpoints.len() == 1 || !requires_target_host(upstream) {
            let index = advance(round_robin, endpoints.len());
            return Ok(&endpoints[index]);
        }
        return Err(DomainError::MissingTargetHost);
    };

    let normalized = requested.trim();
    if !is_bare_host(normalized) {
        return Err(DomainError::InvalidTargetHost(normalized.to_owned()));
    }
    let lowered = normalized.to_ascii_lowercase();
    let matched = endpoints.iter().find(|endpoint| {
        endpoint.host.eq_ignore_ascii_case(&lowered)
            || format!("{}:{}", endpoint.host, endpoint.port).eq_ignore_ascii_case(&lowered)
    });
    match matched {
        Some(endpoint) => Ok(endpoint),
        None => Err(DomainError::UnknownTargetHost(normalized.to_owned())),
    }
}

/// Whether the header must be present.
///
/// A common-suffix alias is one the pool derived from a suffix narrower than
/// any single endpoint's own host: `us.vendor.com` and `eu.vendor.com` derive
/// `vendor.com`, which names neither member. A pool that derives nothing (an
/// explicit alias) or whose derivation is the alias a lone endpoint would have
/// anyway is round-robined instead.
#[must_use]
pub fn requires_target_host(upstream: &Upstream) -> bool {
    let endpoints = &upstream.server.endpoints;
    if endpoints.len() < 2 {
        return false;
    }
    let Some(pooled) = crate::domain::alias::compute_derived_alias(endpoints) else {
        // Nothing was derived, so the alias is an explicit one.
        return false;
    };
    crate::domain::alias::compute_derived_alias(&endpoints[..1]).is_some_and(|single| single != pooled)
}

// The modulo already bounds the counter to `0..total`, so the narrowing casts
// below can never lose a bit; rewriting them as `try_from` would only add an
// unreachable branch to the round-robin walk.
#[allow(clippy::cast_possible_truncation)]
fn advance(counter: &mut u64, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let index = (*counter % total as u64) as usize;
    *counter = counter.wrapping_add(1);
    index
}

/// Whether the value is a hostname or IP with no port, path or specials.
fn is_bare_host(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.contains('/') || value.contains(' ') || value.contains('?') || value.contains('#') {
        return false;
    }
    if value.contains(':') {
        return false;
    }
    if let Some(_host) = value.strip_prefix('[') {
        return false;
    }
    if is_ip_literal(value) {
        return true;
    }
    crate::domain::alias::is_valid_hostname(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Endpoint, Protocol, ServerConfig, Scheme};

    fn endpoint(host: &str) -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: host.to_owned(),
            port: 443,
        }
    }

    fn upstream(hosts: &[&str]) -> Upstream {
        Upstream {
            id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            enabled: true,
            alias: "vendor.com".into(),
            tags: vec![],
            server: ServerConfig {
                endpoints: hosts.iter().map(|host| endpoint(host)).collect(),
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn single_endpoint_needs_no_header() {
        let pool = upstream(&["api.openai.com"]);
        assert!(!requires_target_host(&pool));
        let mut counter = 0u64;
        assert_eq!(select_endpoint(&pool, None, &mut counter).expect("routes"), &endpoint("api.openai.com"));
    }

    #[test]
    fn common_suffix_pool_requires_the_header() {
        let pool = upstream(&["us.vendor.com", "eu.vendor.com"]);
        assert!(requires_target_host(&pool));
        let error = select_endpoint(&pool, None, &mut 0).expect_err("missing");
        assert_eq!(error.status(), 400);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
    }

    #[test]
    fn port_in_the_header_value_is_invalid() {
        let pool = upstream(&["us.vendor.com", "eu.vendor.com"]);
        let error = select_endpoint(&pool, Some("us.vendor.com:8443"), &mut 0).expect_err("port");
        assert_eq!(error.status(), 400);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
        );
    }

    #[test]
    fn unknown_host_is_rejected() {
        let pool = upstream(&["us.vendor.com", "eu.vendor.com"]);
        let error = select_endpoint(&pool, Some("apac.vendor.com"), &mut 0).expect_err("unknown");
        assert_eq!(error.status(), 400);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
        );
    }

    #[test]
    fn valid_host_routes_to_that_endpoint() {
        let pool = upstream(&["us.vendor.com", "eu.vendor.com"]);
        assert_eq!(
            select_endpoint(&pool, Some("eu.vendor.com"), &mut 0).expect("routes"),
            &endpoint("eu.vendor.com")
        );
    }

    #[test]
    fn an_alias_that_names_no_member_requires_the_header() {
        // `vendor.com` is narrower than either member's own host, so the alias
        // alone cannot say which one to reach.
        let pool = upstream(&["us.vendor.com", "eu.vendor.com"]);
        assert!(requires_target_host(&pool));
    }

    #[test]
    fn a_pool_whose_alias_is_a_members_own_host_round_robins() {
        // `vendor.com` *is* the second endpoint, so the alias names a real
        // member and round-robin has nothing to disambiguate.
        let pool = upstream(&["vendor.com", "us.vendor.com"]);
        assert!(!requires_target_host(&pool));
        let mut counter = 0u64;
        let first = select_endpoint(&pool, None, &mut counter).expect("first");
        let second = select_endpoint(&pool, None, &mut counter).expect("second");
        assert_ne!(first.host, second.host);
    }

    #[test]
    fn an_underivable_pool_keeps_its_explicit_alias_and_round_robins() {
        // `a.test` and `b.test` share only the public suffix `test`, so the
        // pool derives nothing: the alias was supplied explicitly.
        let pool = upstream(&["a.test", "b.test"]);
        assert!(!requires_target_host(&pool));
        let mut counter = 0u64;
        let first = select_endpoint(&pool, None, &mut counter).expect("first");
        let second = select_endpoint(&pool, None, &mut counter).expect("second");
        assert_ne!(first.host, second.host);
    }

    #[test]
    fn ip_pools_round_robin() {
        let pool = upstream(&["10.0.0.1", "10.0.0.2"]);
        assert!(!requires_target_host(&pool));
        let mut counter = 0u64;
        let first = select_endpoint(&pool, None, &mut counter).expect("first");
        let second = select_endpoint(&pool, None, &mut counter).expect("second");
        assert_ne!(first.host, second.host);
    }

    #[test]
    fn header_is_consumed_on_read() {
        let mut headers = http::HeaderMap::new();
        headers.insert(TARGET_HOST_HEADER, http::HeaderValue::from_static("us.vendor.com"));
        assert_eq!(take_target_host(&mut headers).as_deref(), Some("us.vendor.com"));
        assert!(headers.get(TARGET_HOST_HEADER).is_none());
    }
}
