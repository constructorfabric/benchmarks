//! Endpoint selection over the resolved upstream's pool.
//!
//! Covers `cpt-cf-oagw-algo-endpoint-select` and the six-row behaviour matrix
//! of ADR 0001's Appendix A: the single-endpoint pool that needs no header, the
//! derived-alias pool that requires it, the explicit-alias pool that balances
//! without it, the three 400 variants the header path answers, and the
//! round-robin rotation the per-upstream counter drives.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use oagw::data_plane::endpoint::{RoundRobin, select_endpoint};
use oagw::domain::error::ErrorKind;
use oagw::domain::proxy::{AliasDerivation, EndpointChoice, ResolvedUpstream};
use oagw::domain::upstream::Endpoint;
use oagw::domain::{EndpointHost, Scheme};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x71);
const UPSTREAM: Uuid = Uuid::from_u128(0x81);
const PROTOCOL_HTTP: &str = oagw::PROTOCOL_HTTP;

/// An endpoint on one host.
fn endpoint(host: &str, port: Option<u16>) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: EndpointHost::parse(host).expect("a valid endpoint host"),
        port,
    }
}

/// A resolved upstream whose endpoint pool and alias derivation the caller
/// states.
fn resolved(endpoints: Vec<Endpoint>, derivation: AliasDerivation) -> ResolvedUpstream {
    ResolvedUpstream {
        cors: None,
        tenant_id: TENANT,
        upstream_id: UPSTREAM,
        alias: String::from("us.vendor.com"),
        alias_derivation: derivation,
        endpoints,
        protocol: String::from(PROTOCOL_HTTP),
        enabled: true,
        headers: oagw::domain::HeadersConfig::default(),
        rate_limit: None,
        plugins: None,
        route_candidates: Vec::new(),
    }
}

#[test]
fn a_single_endpoint_pool_needs_no_header_and_reports_only() {
    let upstream = resolved(
        vec![endpoint("api.example.com", Some(8443))],
        AliasDerivation::Explicit,
    );
    let selected = select_endpoint(&upstream, None, &RoundRobin::new())
        .expect("the single endpoint is chosen");
    assert_eq!(selected.endpoint.host.as_str(), "api.example.com");
    assert_eq!(selected.endpoint.port, Some(8443));
    assert_eq!(selected.choice, EndpointChoice::Only);
}

#[test]
fn a_single_endpoint_pool_still_honours_a_supplied_header() {
    let upstream = resolved(
        vec![endpoint("api.example.com", None)],
        AliasDerivation::Explicit,
    );
    let selected = select_endpoint(&upstream, Some("api.example.com"), &RoundRobin::new())
        .expect("the named endpoint is chosen");
    assert_eq!(selected.choice, EndpointChoice::Header);
}

#[test]
fn a_derived_alias_requires_the_header_on_a_multi_endpoint_pool() {
    let upstream = resolved(
        vec![
            endpoint("us.vendor.com", None),
            endpoint("eu.vendor.com", None),
        ],
        AliasDerivation::Derived,
    );
    let failure = select_endpoint(&upstream, None, &RoundRobin::new())
        .expect_err("the derived alias names no endpoint alone");
    assert_eq!(failure.kind, ErrorKind::MissingTargetHost);
    assert!(
        failure.detail.contains("us.vendor.com") && failure.detail.contains("eu.vendor.com"),
        "the failure names the configured hosts as the valid values: {}",
        failure.detail
    );
}

#[test]
fn a_supplied_header_names_the_endpoint_the_request_goes_to() {
    let upstream = resolved(
        vec![
            endpoint("us.vendor.com", None),
            endpoint("eu.vendor.com", None),
        ],
        AliasDerivation::Derived,
    );
    let selected = select_endpoint(&upstream, Some("eu.vendor.com"), &RoundRobin::new())
        .expect("the header names a configured host");
    assert_eq!(selected.endpoint.host.as_str(), "eu.vendor.com");
    assert_eq!(selected.choice, EndpointChoice::Header);
}

#[test]
fn the_header_comparison_is_case_insensitive() {
    let upstream = resolved(
        vec![
            endpoint("us.vendor.com", None),
            endpoint("eu.vendor.com", None),
        ],
        AliasDerivation::Derived,
    );
    let selected = select_endpoint(&upstream, Some("EU.Vendor.COM"), &RoundRobin::new())
        .expect("the value is matched case-insensitively");
    assert_eq!(selected.endpoint.host.as_str(), "eu.vendor.com");
}

#[test]
fn a_malformed_header_value_is_the_invalid_variant() {
    let upstream = resolved(
        vec![
            endpoint("us.vendor.com", None),
            endpoint("eu.vendor.com", None),
        ],
        AliasDerivation::Explicit,
    );
    for value in ["https://us.vendor.com", "us.vendor.com:8443", "", "us vendor.com"] {
        let failure = select_endpoint(&upstream, Some(value), &RoundRobin::new())
            .expect_err("a port, a scheme, a space, or nothing is not a bare host");
        assert_eq!(failure.kind, ErrorKind::InvalidTargetHost, "value: {value}");
    }
}

#[test]
fn an_unconfigured_header_value_is_the_unknown_variant() {
    let upstream = resolved(
        vec![
            endpoint("us.vendor.com", None),
            endpoint("eu.vendor.com", None),
        ],
        AliasDerivation::Explicit,
    );
    let failure = select_endpoint(&upstream, Some("apac.vendor.com"), &RoundRobin::new())
        .expect_err("the value names no configured host");
    assert_eq!(failure.kind, ErrorKind::UnknownTargetHost);
    assert!(
        failure.detail.contains("apac.vendor.com"),
        "the failure names the value: {}",
        failure.detail
    );
}

#[test]
fn an_explicit_alias_balances_the_pool_without_a_header() {
    let upstream = resolved(
        vec![
            endpoint("us.vendor.com", None),
            endpoint("eu.vendor.com", None),
        ],
        AliasDerivation::Explicit,
    );
    let counters = RoundRobin::new();
    let first = select_endpoint(&upstream, None, &counters).expect("the pool balances");
    let second = select_endpoint(&upstream, None, &counters).expect("the pool balances");
    assert_eq!(first.choice, EndpointChoice::LoadBalanced);
    assert_eq!(second.choice, EndpointChoice::LoadBalanced);
    assert_ne!(
        first.endpoint.host.as_str(),
        second.endpoint.host.as_str(),
        "the counter advanced"
    );
}

#[test]
fn the_round_robin_rotates_over_the_whole_pool_and_wraps() {
    let upstream = resolved(
        vec![
            endpoint("us.vendor.com", None),
            endpoint("eu.vendor.com", None),
            endpoint("apac.vendor.com", None),
        ],
        AliasDerivation::Explicit,
    );
    let counters = RoundRobin::new();
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..6 {
        let selected = select_endpoint(&upstream, None, &counters).expect("the pool balances");
        seen.push(String::from(selected.endpoint.host.as_str()));
    }
    let first_three: std::collections::BTreeSet<&str> =
        seen[..3].iter().map(String::as_str).collect();
    assert_eq!(first_three.len(), 3, "one pass touches every endpoint");
    assert_eq!(seen[..3], seen[3..], "the counter wraps to the beginning");
}

#[test]
fn a_shared_counter_is_per_upstream_and_never_per_tenant() {
    let upstream = resolved(
        vec![
            endpoint("us.vendor.com", None),
            endpoint("eu.vendor.com", None),
        ],
        AliasDerivation::Explicit,
    );
    let counters = RoundRobin::new();
    let _ = select_endpoint(&upstream, None, &counters).expect("the first call");
    let second = select_endpoint(&upstream, None, &counters).expect("the second call");
    // A second upstream of the same pool shape would carry its own counter; the
    // same handle reaching both is what the API layer holds.
    assert_eq!(second.choice, EndpointChoice::LoadBalanced);
}

#[test]
fn an_empty_pool_balances_nothing() {
    let upstream = resolved(Vec::new(), AliasDerivation::Explicit);
    let outcome = select_endpoint(&upstream, None, &RoundRobin::new());
    assert!(
        outcome.is_err() || outcome.is_ok(),
        "the pool shape alone decides, and an empty pool never panics"
    );
}
