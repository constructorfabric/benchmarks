//! Tests for the hop: URI building, header framing and upgrade detection.
use http::{HeaderMap, HeaderValue, Method};

use super::{
    build_request, is_upgrade_request, prepare_request_headers, prepare_response_headers,
    set_origin_form, upstream_uri,
};
use crate::domain::model::{Endpoint, EndpointScheme};
use crate::infra::proxy::failure::{ErrorSource, ProxyFailure};
use crate::infra::proxy::headers::HOP_BY_HOP;

fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint::new(scheme, host, Some(port)).expect("endpoint")
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            http::HeaderName::try_from(*name).expect("name"),
            http::HeaderValue::try_from(*value).expect("value"),
        );
    }
    map
}

#[test]
fn the_upstream_uri_carries_scheme_authority_path_and_query() {
    let endpoint = endpoint(EndpointScheme::Https, "api.example", 443);
    let uri = upstream_uri(&endpoint, "/v1/charges", Some("limit=10")).expect("uri");
    assert_eq!(uri.scheme_str(), Some("https"));
    assert_eq!(
        uri.authority().map(|authority| authority.as_str()),
        Some("api.example:443")
    );
    assert_eq!(uri.path(), "/v1/charges");
    assert_eq!(uri.query(), Some("limit=10"));
}

#[test]
fn a_query_is_omitted_when_absent_or_empty() {
    let endpoint = endpoint(EndpointScheme::Http, "api.example", 80);
    assert_eq!(
        upstream_uri(&endpoint, "/v1", None).expect("uri").query(),
        None
    );
    assert_eq!(
        upstream_uri(&endpoint, "/v1", Some(""))
            .expect("uri")
            .query(),
        None
    );
}

#[test]
fn a_path_is_always_absolute() {
    let endpoint = endpoint(EndpointScheme::Http, "api.example", 80);
    let uri = upstream_uri(&endpoint, "v1/charges", None).expect("uri");
    assert_eq!(uri.path(), "/v1/charges");
}

#[test]
fn an_ipv6_host_is_wrapped_in_brackets() {
    let endpoint = endpoint(EndpointScheme::Http, "::1", 8080);
    let uri = upstream_uri(&endpoint, "/", None).expect("uri");
    assert_eq!(
        uri.authority().map(|authority| authority.as_str()),
        Some("[::1]:8080")
    );
}

#[test]
fn a_plaintext_endpoint_yields_an_http_scheme() {
    let endpoint = endpoint(EndpointScheme::Http, "ws.example", 80);
    let uri = upstream_uri(&endpoint, "/", None).expect("uri");
    assert_eq!(uri.scheme_str(), Some("http"));
}

#[test]
fn an_invalid_authority_is_a_validation_failure() {
    // A host that bypassed endpoint validation would still have to fail here.
    let endpoint = Endpoint {
        scheme: EndpointScheme::Http,
        host: "bad host".to_owned(),
        port: 80,
    };
    let failure = upstream_uri(&endpoint, "/", None).expect_err("validation");
    assert_eq!(failure.status, 400);
    assert_eq!(failure.source, ErrorSource::Gateway);
}

#[test]
fn the_host_header_uses_the_authority_form() {
    let endpoint = endpoint(EndpointScheme::Https, "api.example", 8443);
    let request = build_request(
        &Method::GET,
        upstream_uri(&endpoint, "/v1", None).expect("uri"),
        HeaderMap::new(),
        &endpoint,
        axum::body::Body::empty(),
    )
    .expect("request");
    assert_eq!(request.headers().get("host").unwrap(), "api.example:8443");
}

#[test]
fn the_standard_port_is_omitted_from_the_host_header() {
    let endpoint = endpoint(EndpointScheme::Https, "api.example", 443);
    let request = build_request(
        &Method::GET,
        upstream_uri(&endpoint, "/v1", None).expect("uri"),
        HeaderMap::new(),
        &endpoint,
        axum::body::Body::empty(),
    )
    .expect("request");
    assert_eq!(request.headers().get("host").unwrap(), "api.example");
}

#[test]
fn the_method_and_forwarded_headers_are_carried() {
    let endpoint = endpoint(EndpointScheme::Https, "api.example", 443);
    let forwarded = headers(&[("authorization", "Bearer t"), ("x-trace-id", "trace")]);
    let request = build_request(
        &Method::POST,
        upstream_uri(&endpoint, "/v1", None).expect("uri"),
        forwarded,
        &endpoint,
        axum::body::Body::empty(),
    )
    .expect("request");
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.headers().get("authorization").unwrap(), "Bearer t");
    assert_eq!(request.headers().get("x-trace-id").unwrap(), "trace");
}

#[test]
fn request_headers_are_rebuilt_for_the_outbound_leg() {
    let outbound = headers(&[
        ("host", "gateway.example"),
        ("x-oagw-target-host", "a.example"),
        ("content-length", "12"),
        ("authorization", "Bearer t"),
    ]);
    let prepared = prepare_request_headers(outbound);
    assert!(prepared.get("host").is_none());
    assert!(prepared.get("x-oagw-target-host").is_none());
    assert!(prepared.get("content-length").is_none());
    assert_eq!(prepared.get("authorization").unwrap(), "Bearer t");
}

#[test]
fn response_headers_lose_their_hop_by_hop_members() {
    let inbound = headers(&[
        ("connection", "keep-alive"),
        ("keep-alive", "timeout=5"),
        ("transfer-encoding", "chunked"),
        ("content-type", "application/json"),
    ]);
    let prepared = prepare_response_headers(inbound, false);
    for name in HOP_BY_HOP {
        assert!(
            prepared.get(*name).is_none(),
            "{name} must not travel back to the caller"
        );
    }
    assert_eq!(prepared.get("content-type").unwrap(), "application/json");
}

#[test]
fn an_upgrade_response_keeps_its_upgrade_headers() {
    let inbound = headers(&[("connection", "upgrade"), ("upgrade", "websocket")]);
    let prepared = prepare_response_headers(inbound, true);
    assert_eq!(prepared.get("upgrade").unwrap(), "websocket");
    assert_eq!(prepared.get("connection").unwrap(), "upgrade");
}

#[test]
fn upgrade_detection_needs_both_headers() {
    let both = headers(&[("connection", "upgrade"), ("upgrade", "websocket")]);
    assert!(is_upgrade_request(&both, &Method::GET));

    let missing_connection = headers(&[("upgrade", "websocket")]);
    assert!(!is_upgrade_request(&missing_connection, &Method::GET));

    let missing_upgrade = headers(&[("connection", "upgrade")]);
    assert!(!is_upgrade_request(&missing_upgrade, &Method::GET));

    let unrelated = headers(&[("connection", "keep-alive"), ("upgrade", "websocket")]);
    assert!(!is_upgrade_request(&unrelated, &Method::GET));
}

#[test]
fn connect_is_always_an_upgrade() {
    let empty = HeaderMap::new();
    assert!(is_upgrade_request(&empty, &Method::CONNECT));
}

#[test]
fn an_empty_upgrade_value_is_not_an_upgrade() {
    let mut request = HeaderMap::new();
    request.insert("connection", HeaderValue::from_static("upgrade"));
    request.insert("upgrade", HeaderValue::from_static(""));
    assert!(!is_upgrade_request(&request, &Method::GET));
}

#[test]
fn a_validation_failure_is_a_gateway_decision() {
    let failure = ProxyFailure::validation("nope");
    assert_eq!(failure.status, 400);
    assert_eq!(failure.source, ErrorSource::Gateway);
    assert!(failure.detail.contains("nope"));
}

#[test]
fn the_upstream_target_is_origin_form() {
    let uri = http::Uri::builder()
        .scheme("http")
        .authority("127.0.0.1:9999")
        .path_and_query("/index.html?x=1")
        .build()
        .expect("valid uri");
    let mut request = http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("host", "127.0.0.1:9999")
        .body(())
        .expect("valid request");
    set_origin_form(&mut request);
    assert_eq!(request.uri().path(), "/index.html");
    assert_eq!(request.uri().query(), Some("x=1"));
    assert!(request.uri().authority().is_none());
}

#[test]
fn a_root_target_stays_origin_form() {
    let uri = http::Uri::builder()
        .scheme("https")
        .authority("api.example.com")
        .path_and_query("/")
        .build()
        .expect("valid uri");
    let mut request = http::Request::builder()
        .method("POST")
        .uri(uri)
        .body(())
        .expect("valid request");
    set_origin_form(&mut request);
    assert_eq!(request.uri().path(), "/");
    assert_eq!(request.uri().authority(), None);
}

#[test]
fn an_already_relative_target_is_untouched() {
    let mut request = http::Request::builder()
        .uri("/v1/chat")
        .body(())
        .expect("valid request");
    set_origin_form(&mut request);
    assert_eq!(request.uri().path(), "/v1/chat");
}
