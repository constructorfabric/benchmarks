//! Tests for the transport layer: path splitting and upgrade handling.
use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;

use super::{PROXY_PATH, PROXY_PREFIX, proxied_methods, split_path, uri_parts};
use crate::infra::proxy::{ErrorSource, ProxyFailure};

#[test]
fn the_proxy_mounts_under_the_version_prefix_without_the_api_prefix() {
    assert!(PROXY_PATH.starts_with("/oagw/v1/proxy/"));
    assert!(!PROXY_PATH.starts_with("/api"));
    assert_eq!(PROXY_PATH, "/oagw/v1/proxy/{*proxy_path}");
    assert_eq!(PROXY_PREFIX, "/oagw/v1/proxy/");
}

#[test]
fn the_prefix_alone_carries_no_alias() {
    let failure = split_path("/oagw/v1/proxy/").expect_err("validation");
    assert_eq!(failure.status, 400);
    assert!(failure.detail.contains("alias"));
}

#[test]
fn a_request_outside_the_prefix_is_refused() {
    for raw in [
        "/oagw/v1",
        "/oagw/v1/upstreams",
        "/api/oagw/v1/proxy/payments",
        "/",
    ] {
        let failure = split_path(raw).expect_err(raw);
        assert_eq!(failure.status, 400, "{raw}");
        assert_eq!(failure.source, ErrorSource::Gateway);
    }
}

#[test]
fn an_alias_without_a_path_forwards_the_root() {
    let (alias, path) = split_path("/oagw/v1/proxy/payments").expect("split");
    assert_eq!(alias, "payments");
    assert_eq!(path, "/");
}

#[test]
fn an_alias_and_a_path_are_split_at_the_first_slash() {
    let (alias, path) = split_path("/oagw/v1/proxy/payments/v1/charges/42").expect("split");
    assert_eq!(alias, "payments");
    assert_eq!(path, "/v1/charges/42");
}

#[test]
fn an_encoded_slash_stays_in_the_alias_segment() {
    // The handler reads the raw URI path, so percent escapes are not decoded
    // before the alias is extracted.
    let (alias, path) = split_path("/oagw/v1/proxy/payments/a%2Fb").expect("split");
    assert_eq!(alias, "payments");
    assert_eq!(path, "/a%2Fb");
}

#[test]
fn a_trailing_slash_belongs_to_the_path() {
    let (alias, path) = split_path("/oagw/v1/proxy/payments/").expect("split");
    assert_eq!(alias, "payments");
    assert_eq!(path, "/");
}

#[test]
fn the_alias_cannot_contain_a_slash() {
    let (alias, _path) = split_path("/oagw/v1/proxy/payments/v1").expect("split");
    assert!(!alias.contains('/'));
}

#[test]
fn every_proxied_method_is_documented_and_is_a_real_method() {
    let methods = proxied_methods();
    assert_eq!(methods.len(), 7);
    for method in &methods {
        assert_ne!(*method, Method::CONNECT, "CONNECT is refused, not proxied");
    }
    for expected in ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
        assert!(
            methods.iter().any(|method| method.as_str() == expected),
            "{expected} must be declared"
        );
    }
}

#[test]
fn the_methods_are_upper_case_on_the_wire() {
    for method in proxied_methods() {
        assert_eq!(method.as_str(), method.as_str().to_ascii_uppercase());
    }
}

#[test]
fn the_uri_is_split_without_decoding() {
    let uri: Uri = "/oagw/v1/proxy/payments/v1/charges?limit=10&cursor=abc"
        .parse()
        .expect("uri");
    let (path, query) = uri_parts(&uri);
    assert_eq!(path, "/oagw/v1/proxy/payments/v1/charges");
    assert_eq!(query, Some("limit=10&cursor=abc"));
}

#[test]
fn a_uri_without_a_query_has_none() {
    let uri: Uri = "/oagw/v1/proxy/payments".parse().expect("uri");
    assert_eq!(uri_parts(&uri).1, None);
}

#[test]
fn a_malformed_outbound_response_renders_an_internal_problem() {
    // A response the gateway cannot render is a gateway failure, not an
    // upstream one.
    let failure = ProxyFailure::internal("cannot render");
    assert_eq!(failure.status, 500);
    assert_eq!(failure.source, ErrorSource::Gateway);
    let response = crate::api::proxy::error::ProxyError::new(failure).into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
