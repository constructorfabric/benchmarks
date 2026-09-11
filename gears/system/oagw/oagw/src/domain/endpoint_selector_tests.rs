//! Tests of endpoint selection and the target-host behaviour matrix
//! (`cpt-cf-oagw-flow-request-proxy-target-host-selection`,
//! `cpt-cf-oagw-algo-request-proxy-endpoint-select`).

use crate::domain::endpoints::*;
use crate::domain::dto::{Endpoint, EndpointScheme, ServerConfig};
use crate::domain::DomainError;

fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint { scheme, host: host.to_owned(), port }
}

fn pool(endpoints: Vec<Endpoint>) -> ServerConfig {
    ServerConfig { endpoints }
}

fn select_with(
    server: &ServerConfig,
    alias: &str,
    target_host: Option<&str>,
    allow_http: bool,
    next_index: usize,
) -> Result<SelectedEndpoint, DomainError> {
    select_endpoint(
        server,
        Some("upstream".to_owned()),
        alias,
        target_host,
        allow_http,
        None,
        |_| next_index,
    )
}

#[test]
fn a_one_endpoint_pool_is_single() {
    let server = pool(vec![endpoint(EndpointScheme::Https, "api.vendor.com", 443)]);
    assert_eq!(alias_shape(&server, "api.vendor.com"), AliasShape::Single);
}

#[test]
fn a_multi_endpoint_pool_with_the_derived_alias_is_a_common_suffix() {
    let server = pool(vec![
        endpoint(EndpointScheme::Https, "eu.api.vendor.com", 443),
        endpoint(EndpointScheme::Https, "us.api.vendor.com", 443),
    ]);
    assert_eq!(alias_shape(&server, "api.vendor.com"), AliasShape::CommonSuffix);
}

#[test]
fn a_multi_endpoint_pool_with_an_explicit_alias_is_explicit() {
    let server = pool(vec![
        endpoint(EndpointScheme::Https, "eu.api.vendor.com", 443),
        endpoint(EndpointScheme::Https, "us.api.vendor.com", 443),
    ]);
    assert_eq!(alias_shape(&server, "vendor-pool"), AliasShape::Explicit);
}

#[test]
fn a_single_pool_selects_its_only_endpoint() {
    let server = pool(vec![endpoint(EndpointScheme::Https, "api.vendor.com", 443)]);
    let selected = select_with(&server, "api.vendor.com", None, true, 7).expect("selected");
    assert_eq!(selected.endpoint.host, "api.vendor.com");
    assert_eq!(selected.method, SelectionMethod::Default);
}

#[test]
fn an_explicit_multi_endpoint_pool_rotates() {
    let server = pool(vec![
        endpoint(EndpointScheme::Https, "eu.api.vendor.com", 443),
        endpoint(EndpointScheme::Https, "us.api.vendor.com", 443),
    ]);
    let selected = select_with(&server, "vendor-pool", None, true, 0).expect("selected");
    assert_eq!(selected.endpoint.host, "eu.api.vendor.com");
    assert_eq!(selected.method, SelectionMethod::RoundRobin);
    let selected = select_with(&server, "vendor-pool", None, true, 1).expect("selected");
    assert_eq!(selected.endpoint.host, "us.api.vendor.com");
}

#[test]
fn a_common_suffix_pool_without_a_target_host_is_rejected() {
    let server = pool(vec![
        endpoint(EndpointScheme::Https, "eu.api.vendor.com", 443),
        endpoint(EndpointScheme::Https, "us.api.vendor.com", 443),
    ]);
    let error = select_with(&server, "api.vendor.com", None, true, 0).expect_err("mandatory");
    assert!(matches!(error, DomainError::MissingTargetHost { .. }));
}

#[test]
fn a_supplied_target_host_bypasses_the_rotation() {
    let server = pool(vec![
        endpoint(EndpointScheme::Https, "eu.api.vendor.com", 443),
        endpoint(EndpointScheme::Https, "us.api.vendor.com", 443),
    ]);
    let selected = select_with(&server, "vendor-pool", Some("us.api.vendor.com"), true, 0).expect("selected");
    assert_eq!(selected.endpoint.host, "us.api.vendor.com");
    assert_eq!(selected.method, SelectionMethod::ExplicitHeader);
}

#[test]
fn a_target_host_value_is_always_validated_whatever_the_shape() {
    let server = pool(vec![endpoint(EndpointScheme::Https, "api.vendor.com", 443)]);
    let error = select_with(&server, "api.vendor.com", Some("other.vendor.com"), true, 0)
        .expect_err("an unknown host");
    assert!(matches!(error, DomainError::UnknownTargetHost { .. }));
    let error = select_with(&server, "api.vendor.com", Some("api.vendor.com:8443"), true, 0)
        .expect_err("a port is not a bare host");
    assert!(matches!(error, DomainError::InvalidTargetHost { .. }));
}

#[test]
fn a_target_host_value_is_matched_case_insensitively_and_without_a_trailing_dot() {
    let server = pool(vec![endpoint(EndpointScheme::Https, "api.vendor.com", 443)]);
    let selected = select_with(&server, "api.vendor.com", Some("API.Vendor.COM."), true, 0).expect("selected");
    assert_eq!(selected.endpoint.host, "api.vendor.com");
}

#[test]
fn a_bare_hostname_or_ip_address_is_a_valid_target_host_value() {
    assert!(target_host_is_valid("api.vendor.com"));
    assert!(target_host_is_valid("10.0.0.1"));
    assert!(!target_host_is_valid(""));
    assert!(!target_host_is_valid("api.vendor.com/path"));
    assert!(!target_host_is_valid("api.vendor.com:443"));
    assert!(!target_host_is_valid("api.vendor.com x"));
    assert!(!target_host_is_valid("api.vendor.com#f"));
}

#[test]
fn a_non_uniform_pool_is_rejected() {
    let server = pool(vec![
        endpoint(EndpointScheme::Https, "eu.api.vendor.com", 443),
        endpoint(EndpointScheme::Https, "us.api.vendor.com", 8443),
    ]);
    assert!(require_uniform_pool(&server).is_err());
    assert!(require_uniform_pool(&pool(Vec::new())).is_err());
}

#[test]
fn the_valid_hosts_of_a_pool_are_sorted_and_deduplicated() {
    let server = pool(vec![
        endpoint(EndpointScheme::Https, "us.api.vendor.com", 443),
        endpoint(EndpointScheme::Https, "eu.api.vendor.com", 443),
        endpoint(EndpointScheme::Https, "eu.api.vendor.com", 443),
    ]);
    assert_eq!(valid_hosts(&server), vec!["eu.api.vendor.com", "us.api.vendor.com"]);
}

#[test]
fn http_is_admitted_exactly_while_the_allowlist_admits_it() {
    let http = endpoint(EndpointScheme::Http, "api.vendor.com", 80);
    assert!(admit(http.clone(), SelectionMethod::Default, true, None).is_ok());
    let error = admit(http, SelectionMethod::Default, false, None).expect_err("http is not admitted");
    assert!(format!("{error}").contains("scheme"));
    let https = endpoint(EndpointScheme::Https, "api.vendor.com", 443);
    assert!(admit(https, SelectionMethod::Default, false, None).is_ok());
}

#[test]
fn the_scheme_names_are_the_wire_values() {
    assert_eq!(scheme_name(EndpointScheme::Http), "http");
    assert_eq!(scheme_name(EndpointScheme::Https), "https");
    assert_eq!(scheme_name(EndpointScheme::Wss), "wss");
    assert_eq!(scheme_name(EndpointScheme::Wt), "wt");
    assert_eq!(scheme_name(EndpointScheme::Grpc), "grpc");
}

#[test]
fn the_selection_methods_name_how_the_endpoint_was_chosen() {
    assert_eq!(SelectionMethod::ExplicitHeader.as_str(), "explicit_header");
    assert_eq!(SelectionMethod::RoundRobin.as_str(), "round_robin");
    assert_eq!(SelectionMethod::Default.as_str(), "default");
}
