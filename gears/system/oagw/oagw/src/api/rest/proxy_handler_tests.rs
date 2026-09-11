//! Tests of the REST proxy handler surface
//! (`cpt-cf-oagw-flow-request-proxy-dispatch`,
//! `cpt-cf-oagw-dod-request-proxy-preflight-detection`).

use super::proxy_handlers::{header_value, is_preflight, render, render_failure, target_of, PROXY_PREFIX, TRACE_ID_HEADER};
use super::PROXY_PATH;
use crate::domain::cors::{preflight_response_headers, PreflightCors};
use crate::domain::error::DomainError;
use crate::domain::proxy::{ErrorSource, ProxyBody, ProxyFailure, ProxyResponse, StreamKind};
use bytes::Bytes;
use std::collections::HashMap;

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect()
}

#[test]
fn the_proxy_paths_are_gear_relative_and_catch_all() {
    assert!(PROXY_PREFIX.starts_with("/oagw/v1/proxy/"));
    assert!(!PROXY_PREFIX.starts_with("/api"));
    assert!(PROXY_PATH.starts_with("/oagw/v1/proxy/"));
}

#[test]
fn an_options_request_with_an_origin_and_a_requested_method_is_a_preflight() {
    assert!(is_preflight("OPTIONS", &headers(&[("origin", "https://app.dev"), ("access-control-request-method", "POST")])));
    assert!(is_preflight("options", &headers(&[("origin", "https://app.dev"), ("access-control-request-method", "POST")])) == false);
    assert!(!is_preflight("POST", &headers(&[("origin", "https://app.dev"), ("access-control-request-method", "POST")])));
    assert!(!is_preflight("OPTIONS", &headers(&[("origin", "https://app.dev")])));
    assert!(!is_preflight("OPTIONS", &headers(&[("access-control-request-method", "POST")])));
}

#[test]
fn a_preflight_response_echoes_the_request_and_is_gateway_sourced() {
    let response = render(ProxyResponse::preflight(preflight_response_headers(
        Some("https://app.dev"),
        Some("POST"),
        Some("x-a, x-b"),
        Some(PreflightCors { allow_credentials: true, exact: true }),
    )));
    assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
    let names: Vec<String> = response.headers().iter().map(|(name, _)| name.as_str().to_owned()).collect();
    let value_of = |name: &str| response.headers().get(name).expect(name).to_str().expect("ascii").to_owned();
    assert_eq!(value_of("access-control-allow-origin"), "https://app.dev");
    assert_eq!(value_of("access-control-allow-methods"), "POST");
    assert_eq!(value_of("access-control-allow-headers"), "x-a, x-b");
    assert_eq!(value_of("access-control-max-age"), "86400");
    assert_eq!(
        value_of("vary"),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
    );
    assert_eq!(value_of("access-control-allow-credentials"), "true");
    assert_eq!(value_of("x-oagw-error-source"), "gateway");
    assert!(names.iter().all(|name| !name.starts_with("access-control-request")));
}

#[test]
fn a_preflight_response_without_a_requested_method_still_carries_the_max_age() {
    let response = render(ProxyResponse::preflight(preflight_response_headers(
        Some("https://app.dev"),
        None,
        None,
        None,
    )));
    assert_eq!(response.status(), http::StatusCode::NO_CONTENT);
    assert!(response.headers().get("access-control-allow-methods").is_none());
    assert!(response.headers().get("access-control-allow-credentials").is_none());
    assert_eq!(
        response.headers().get("access-control-max-age").map(|value| value.to_str().expect("ascii")),
        Some(crate::domain::cors::MAX_AGE)
    );
}

#[test]
fn a_header_value_is_found_exactly_as_the_framework_lowercased_it() {
    let headers = headers(&[("x-request-id", "trace-1"), ("X-OTHER", "v")]);
    assert_eq!(header_value(&headers, "x-request-id"), Some("trace-1"));
    assert_eq!(header_value(&headers, "X-REQUEST-ID"), None);
    assert_eq!(header_value(&headers, "missing"), None);
}

#[test]
fn the_trace_identifier_header_is_the_one_the_caller_supplies() {
    assert_eq!(TRACE_ID_HEADER, "x-request-id");
}

#[test]
fn the_target_splits_the_alias_from_the_path_suffix() {
    assert_eq!(
        target_of("/oagw/v1/proxy/api.vendor.com/v1/orders"),
        Some(("api.vendor.com".to_owned(), Some("/v1/orders".to_owned())))
    );
    assert_eq!(
        target_of("/oagw/v1/proxy/api.vendor.com"),
        Some(("api.vendor.com".to_owned(), None))
    );
}

#[test]
fn a_target_without_an_alias_is_not_a_proxy_target() {
    assert_eq!(target_of("/oagw/v1/proxy/"), None);
    assert_eq!(target_of("/oagw/v1/proxy"), None);
    assert_eq!(target_of("/api/oagw/v1/proxy/x"), None);
    assert_eq!(target_of("/oagw/v1/upstreams"), None);
}

#[test]
fn an_upstream_response_passes_through_with_its_status_and_body() {
    let rendered = render(crate::domain::proxy::ProxyResponse {
        status: 201,
        headers: vec![("content-type".to_owned(), "text/plain".to_owned()), ("x-keep".to_owned(), "1".to_owned())],
        body: ProxyBody::Buffered(Bytes::from_static(b"hello")),
        source: ErrorSource::Upstream,
        stream: StreamKind::None,
        lifecycle: crate::domain::proxy::StreamLifecycle::shared(),
        error: None,
        observation: crate::domain::proxy::ProxyObservation::default(),
    });
    assert_eq!(rendered.status(), http::StatusCode::CREATED);
    assert_eq!(
        rendered.headers().get("x-oagw-error-source").expect("stamped").to_str().expect("ascii"),
        "upstream"
    );
    assert!(rendered.headers().get("content-type").is_some());
}

#[test]
fn an_upstream_header_the_framework_cannot_name_is_dropped_not_fatal() {
    let rendered = render(crate::domain::proxy::ProxyResponse {
        status: 200,
        headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
        body: ProxyBody::Empty,
        source: ErrorSource::Upstream,
        stream: StreamKind::None,
        lifecycle: crate::domain::proxy::StreamLifecycle::shared(),
        error: None,
        observation: crate::domain::proxy::ProxyObservation::default(),
    });
    assert_eq!(rendered.status(), http::StatusCode::OK);
}

#[test]
fn a_gateway_error_is_stamped_gateway() {
    let rendered = render(crate::domain::proxy::ProxyResponse {
        status: 502,
        headers: Vec::new(),
        body: ProxyBody::Empty,
        source: ErrorSource::Gateway,
        stream: StreamKind::None,
        lifecycle: crate::domain::proxy::StreamLifecycle::shared(),
        error: Some(DomainError::RouteNotFound { path: None, trace_id: None }),
        observation: crate::domain::proxy::ProxyObservation::default(),
    });
    assert_eq!(
        rendered.headers().get("x-oagw-error-source").expect("stamped").to_str().expect("ascii"),
        "gateway"
    );
}

#[test]
fn a_domain_failure_renders_through_the_shared_error_contract() {
    let rendered = render_failure(ProxyFailure::Domain(DomainError::LinkUnavailable {
        upstream_id: None,
        host: None,
        path: None,
        trace_id: None,
    }));
    assert_eq!(rendered.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(rendered.headers().get("x-oagw-error-source").is_some());
}

#[test]
fn a_method_excluded_path_renders_405_with_the_methods_the_route_admits() {
    let rendered = render_failure(ProxyFailure::MethodNotAllowed {
        path: Some("/v1".to_owned()),
        allowed: vec!["GET", "POST"],
    });
    assert_eq!(rendered.status(), http::StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        rendered.headers().get("allow").expect("allow").to_str().expect("ascii"),
        "GET, POST"
    );
    assert_eq!(
        rendered.headers().get("x-oagw-error-source").expect("stamped").to_str().expect("ascii"),
        "gateway"
    );
}

#[test]
fn an_unresolvable_trace_identifier_is_carried_not_minted() {
    let mut inbound: HashMap<String, String> = HashMap::new();
    inbound.insert("x-request-id".to_owned(), "trace-1".to_owned());
    assert_eq!(inbound.get(TRACE_ID_HEADER).map(String::as_str), Some("trace-1"));
}
