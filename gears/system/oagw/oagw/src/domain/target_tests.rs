//! Unit tests for [`super::target`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use uuid::Uuid;

use super::{select_endpoint, SelectionMethod, TARGET_HOST_HEADER};
use crate::domain::error::DomainError;
use crate::domain::models::{Endpoint, EndpointScheme, Protocol, ServerConfig, Upstream};

fn upstream(endpoints: &[(&str, u16)]) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        alias: "vendor.com".to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: endpoints
                .iter()
                .map(|(host, port)| Endpoint::new(EndpointScheme::Https, *host, *port))
                .collect(),
        },
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

#[test]
fn a_single_endpoint_needs_no_target_host_header() {
    let pool = upstream(&[("us.vendor.com", 443)]);
    let selected = select_endpoint(&pool, None, 0).unwrap();
    assert_eq!(selected.endpoint.host, "us.vendor.com");
    assert_eq!(selected.method, SelectionMethod::Default);
}

#[test]
fn an_explicit_header_pins_the_endpoint() {
    let pool = upstream(&[("us.vendor.com", 443), ("eu.vendor.com", 443)]);
    let selected = select_endpoint(&pool, Some("eu.vendor.com"), 0).unwrap();
    assert_eq!(selected.endpoint.host, "eu.vendor.com");
    assert_eq!(selected.method, SelectionMethod::ExplicitHeader);
}

#[test]
fn a_pooled_common_suffix_alias_requires_the_header() {
    let pool = upstream(&[("us.vendor.com", 443), ("eu.vendor.com", 443)]);
    let rejected = select_endpoint(&pool, None, 0).unwrap_err();
    assert!(matches!(rejected, DomainError::MissingTargetHost { .. }));
    assert_eq!(
        rejected.extensions().valid_hosts.unwrap(),
        vec!["us.vendor.com", "eu.vendor.com"]
    );
}

#[test]
fn a_pooled_explicit_alias_round_robins_without_the_header() {
    // IP-based pools cannot derive an alias, so their alias is always explicit.
    let mut pool = upstream(&[("10.0.0.1", 443), ("10.0.0.2", 443)]);
    pool.alias = "ip-pool".to_owned();
    assert_eq!(super::alias_source_of(&pool), crate::domain::alias::AliasSource::Explicit);

    let first = select_endpoint(&pool, None, 0).unwrap();
    assert_eq!(first.method, SelectionMethod::RoundRobin);
    assert_eq!(first.endpoint.host, "10.0.0.1");
    let second = select_endpoint(&pool, None, 1).unwrap();
    assert_eq!(second.endpoint.host, "10.0.0.2");
    let third = select_endpoint(&pool, None, 2).unwrap();
    assert_eq!(third.endpoint.host, "10.0.0.1");
}

#[test]
fn an_unknown_target_host_is_rejected() {
    let pool = upstream(&[("us.vendor.com", 443), ("eu.vendor.com", 443)]);
    let rejected = select_endpoint(&pool, Some("apac.vendor.com"), 0).unwrap_err();
    match rejected {
        DomainError::UnknownTargetHost {
            invalid_value,
            valid_hosts,
            ..
        } => {
            assert_eq!(invalid_value, "apac.vendor.com");
            assert_eq!(valid_hosts.len(), 2);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn a_malformed_target_host_is_rejected() {
    let pool = upstream(&[("us.vendor.com", 443), ("eu.vendor.com", 443)]);
    for value in ["us.vendor.com:8443", "us.vendor.com/x", ".."] {
        let rejected = select_endpoint(&pool, Some(value), 0).unwrap_err();
        assert!(
            matches!(rejected, DomainError::InvalidTargetHost { .. }),
            "expected InvalidTargetHost for '{value}', got {rejected:?}"
        );
    }
}

#[test]
fn target_host_header_matching_is_case_insensitive() {
    let pool = upstream(&[("us.vendor.com", 443), ("eu.vendor.com", 443)]);
    let selected = select_endpoint(&pool, Some("  EU.Vendor.COM. "), 0).unwrap();
    assert_eq!(selected.endpoint.host, "eu.vendor.com");
}

#[test]
fn an_empty_pool_is_an_unavailable_link() {
    let pool = upstream(&[]);
    let rejected = select_endpoint(&pool, None, 0).unwrap_err();
    assert!(matches!(rejected, DomainError::LinkUnavailable { .. }));
}

#[test]
fn the_header_constant_is_the_wire_name() {
    assert_eq!(TARGET_HOST_HEADER, "x-oagw-target-host");
}
