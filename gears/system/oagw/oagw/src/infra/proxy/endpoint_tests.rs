#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use super::endpoint::EndpointSelector;
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme, Protocol, ServerConfig, Upstream};

fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn upstream(endpoints: Vec<Endpoint>, alias: &str) -> Upstream {
    upstream_with_id(endpoints, alias, 0x11)
}

fn upstream_with_id(endpoints: Vec<Endpoint>, alias: &str, id: u128) -> Upstream {
    Upstream {
        id: uuid::Uuid::from_u128(id),
        tenant_id: uuid::Uuid::from_u128(0xC001),
        alias: alias.to_owned(),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig { endpoints },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

/// A single-endpoint upstream needs no target host.
#[test]
fn a_single_endpoint_needs_no_target_host() {
    let row = upstream(
        vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)],
        "api.openai.com",
    );
    let selector = EndpointSelector::default();
    let picked = selector.select(&row, None).unwrap();
    assert_eq!(picked.host, "api.openai.com");
    assert_eq!(picked.port, 443);
}

/// The explicit target host pins an endpoint of a pool.
#[test]
fn an_explicit_target_host_pins_an_endpoint() {
    let row = upstream(
        vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ],
        "vendor.com",
    );
    let selector = EndpointSelector::default();
    let picked = selector.select(&row, Some("eu.vendor.com")).unwrap();
    assert_eq!(picked.host, "eu.vendor.com");
}

/// Endpoint hosts are matched case-insensitively, as DNS names are.
#[test]
fn target_host_matching_is_case_insensitive() {
    let row = upstream(
        vec![
            endpoint(EndpointScheme::Https, "us.Vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ],
        "vendor.com",
    );
    let selector = EndpointSelector::default();
    let picked = selector.select(&row, Some("US.VENDOR.COM")).unwrap();
    assert_eq!(picked.host, "us.Vendor.com");
}

/// A target host that names no endpoint lists the valid hosts (`400`).
#[test]
fn an_unknown_target_host_lists_the_valid_hosts() {
    let row = upstream(
        vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ],
        "vendor.com",
    );
    let selector = EndpointSelector::default();
    let error = selector.select(&row, Some("ap.vendor.com")).unwrap_err();
    match error {
        DomainError::UnknownTargetHost {
            invalid_value,
            valid_hosts,
        } => {
            assert_eq!(invalid_value, "ap.vendor.com");
            assert_eq!(valid_hosts, vec!["us.vendor.com", "eu.vendor.com"]);
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// A target host carrying a port, path or other decoration is malformed
/// (`400`): the port belongs to the endpoint configuration.
#[test]
fn a_malformed_target_host_is_rejected() {
    let row = upstream(
        vec![endpoint(EndpointScheme::Https, "us.vendor.com", 443)],
        "us.vendor.com",
    );
    let selector = EndpointSelector::default();
    for bad in [
        "us.vendor.com:443",
        "us.vendor.com/v1",
        "https://us.vendor.com",
        "us vendor.com",
        "",
    ] {
        let error = selector.select(&row, Some(bad)).unwrap_err();
        assert!(
            matches!(error, DomainError::InvalidTargetHost { .. }),
            "{bad} -> {error:?}"
        );
    }
}

/// A pool whose alias is the shared registrable suffix requires an explicit
/// target host: the alias cannot name one endpoint (`400`).
#[test]
fn a_common_suffix_pool_requires_a_target_host() {
    let row = upstream(
        vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ],
        "vendor.com",
    );
    let selector = EndpointSelector::default();
    let error = selector.select(&row, None).unwrap_err();
    match error {
        DomainError::MissingTargetHost { alias, valid_hosts } => {
            assert_eq!(alias, "vendor.com");
            assert_eq!(valid_hosts, vec!["us.vendor.com", "eu.vendor.com"]);
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// An explicit alias that names one endpoint of the pool pins it.
#[test]
fn an_alias_naming_an_endpoint_pins_it() {
    let row = upstream(
        vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", 443),
            endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
        ],
        "us.vendor.com",
    );
    let selector = EndpointSelector::default();
    let picked = selector.select(&row, None).unwrap();
    assert_eq!(picked.host, "us.vendor.com");
}

/// An explicit (non-derivable) alias over a pool rotates the endpoints
/// round-robin, as `DESIGN` §"Multi-Endpoint Load Balancing" requires.
#[test]
fn an_explicit_alias_pool_round_robins() {
    let row = upstream(
        vec![
            endpoint(EndpointScheme::Http, "10.0.0.1", 8080),
            endpoint(EndpointScheme::Http, "10.0.0.2", 8080),
        ],
        "my-service",
    );
    let selector = Arc::new(EndpointSelector::with_allow_http(true));
    let mut hosts = Vec::new();
    for _ in 0..4 {
        let picked = selector.select(&row, None).unwrap();
        hosts.push(picked.host);
    }
    assert_eq!(hosts, ["10.0.0.1", "10.0.0.2", "10.0.0.1", "10.0.0.2"]);
}

/// Round-robin state is per upstream, so two pools do not steal each other's
/// turn.
#[test]
fn round_robin_state_is_per_upstream() {
    let first = upstream(
        vec![
            endpoint(EndpointScheme::Http, "10.0.0.1", 8080),
            endpoint(EndpointScheme::Http, "10.0.0.2", 8080),
        ],
        "my-service",
    );
    let second = upstream_with_id(
        vec![
            endpoint(EndpointScheme::Http, "10.1.0.1", 8080),
            endpoint(EndpointScheme::Http, "10.1.0.2", 8080),
        ],
        "other-service",
        0x12,
    );
    let selector = EndpointSelector::with_allow_http(true);
    let _ = selector.select(&first, None).unwrap();
    let picked = selector.select(&second, None).unwrap();
    assert_eq!(picked.host, "10.1.0.1");
}

/// `DESIGN` §2.2: plaintext upstreams are refused unless the deployment
/// explicitly allows them.
#[test]
fn a_plaintext_upstream_is_blocked_by_default() {
    let row = upstream(
        vec![endpoint(EndpointScheme::Http, "api.openai.com", 80)],
        "api.openai.com",
    );
    let selector = EndpointSelector::default();
    let error = selector.select(&row, None).unwrap_err();
    assert!(matches!(error, DomainError::LinkUnavailable { .. }));

    let permissive = EndpointSelector::with_allow_http(true);
    assert!(permissive.select(&row, None).is_ok());
}

/// A TLS upstream is never affected by the plaintext switch.
#[test]
fn a_tls_upstream_is_always_allowed() {
    let row = upstream(
        vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)],
        "api.openai.com",
    );
    let strict = EndpointSelector::default();
    assert!(strict.select(&row, None).is_ok());
}

/// The selected endpoint exposes the URL authority the transport must dial.
#[test]
fn the_selected_endpoint_renders_its_authority() {
    let row = upstream(
        vec![endpoint(EndpointScheme::Http, "127.0.0.1", 8080)],
        "loopback",
    );
    let selector = EndpointSelector::with_allow_http(true);
    let picked = selector.select(&row, None).unwrap();
    assert_eq!(picked.authority(), "127.0.0.1:8080");

    let row = upstream(
        vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)],
        "api.openai.com",
    );
    let picked = selector.select(&row, None).unwrap();
    assert_eq!(picked.authority(), "api.openai.com");
    assert_eq!(picked.url(), "https://api.openai.com:443");
    assert_eq!(picked.port, 443);
}

/// The transport needs the URL scheme of the selected endpoint.
#[test]
fn the_selected_endpoint_renders_its_scheme() {
    let http = upstream(
        vec![endpoint(EndpointScheme::Http, "127.0.0.1", 8080)],
        "loopback",
    );
    let https = upstream(
        vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)],
        "api.openai.com",
    );
    let wss = upstream(
        vec![endpoint(EndpointScheme::Wss, "ws.vendor.com", 443)],
        "ws.vendor.com",
    );
    let selector = EndpointSelector::with_allow_http(true);
    assert_eq!(selector.select(&http, None).unwrap().url_scheme(), "http");
    assert_eq!(selector.select(&https, None).unwrap().url_scheme(), "https");
    assert_eq!(selector.select(&wss, None).unwrap().url_scheme(), "https");
}
