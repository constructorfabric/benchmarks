// Created: 2026-09-04 by Constructor Tech
//! Tests of the header transformation of the data plane.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;

use http::{HeaderMap, HeaderValue};

use super::*;
use crate::domain::{HeaderPassthrough, RequestHeaderRules, ResponseHeaderRules};

/// A request rule set that forwards nothing.
fn none_rules() -> RequestHeaderRules {
    RequestHeaderRules {
        set: BTreeMap::new(),
        add: BTreeMap::new(),
        remove: Vec::new(),
        passthrough: HeaderPassthrough::None,
        passthrough_allowlist: Vec::new(),
    }
}

/// A header map built from raw `name: value` pairs.
fn headers_of(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

#[test]
fn hop_by_hop_names_are_recognized() {
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        assert!(is_hop_by_hop(name), "{name} is hop-by-hop");
        assert!(is_hop_by_hop(&name.to_ascii_uppercase()));
    }
    assert!(!is_hop_by_hop("authorization"));
    assert!(!is_hop_by_hop("x-custom"));
}

#[test]
fn connection_header_tokens_are_collected() {
    let mut inbound = HeaderMap::new();
    inbound.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("X-Custom, Te"),
    );
    inbound.append(
        http::header::CONNECTION,
        HeaderValue::from_static("x-also-doomed"),
    );
    assert_eq!(
        connection_tokens(&inbound),
        vec!["x-custom", "te", "x-also-doomed"]
    );
}

#[test]
fn strip_hop_by_hop_removes_the_fixed_list_and_connection_tokens() {
    let mut inbound = headers_of(&[
        ("connection", "x-doomed"),
        ("x-doomed", "yes"),
        ("te", "trailers"),
        ("transfer-encoding", "chunked"),
        ("authorization", "Bearer keep-me"),
    ]);
    strip_hop_by_hop(&mut inbound);
    assert!(!inbound.contains_key("connection"));
    assert!(!inbound.contains_key("x-doomed"));
    assert!(!inbound.contains_key("te"));
    assert!(!inbound.contains_key("transfer-encoding"));
    assert_eq!(inbound.get("authorization").unwrap(), "Bearer keep-me");
}

#[test]
fn strip_routing_headers_removes_target_host_and_content_length() {
    let mut inbound = headers_of(&[
        ("host", "api.example.com"),
        (TARGET_HOST_HEADER.as_str(), "api.example.com"),
        ("content-length", "12"),
        ("x-kept", "yes"),
    ]);
    strip_routing_headers(&mut inbound);
    assert!(!inbound.contains_key("host"));
    assert!(!inbound.contains_key(TARGET_HOST_HEADER.as_str()));
    assert!(!inbound.contains_key("content-length"));
    assert_eq!(inbound.get("x-kept").unwrap(), "yes");
}

#[test]
fn target_host_is_read_trimmed_and_optional() {
    let empty = HeaderMap::new();
    assert!(target_host(&empty).is_none());

    let mut inbound = HeaderMap::new();
    inbound.insert(
        TARGET_HOST_HEADER,
        HeaderValue::from_static("  host.internal  "),
    );
    assert_eq!(target_host(&inbound), Some("host.internal"));

    let mut blank = HeaderMap::new();
    blank.insert(TARGET_HOST_HEADER, HeaderValue::from_static("   "));
    assert!(target_host(&blank).is_none());
}

#[test]
fn target_host_validation_accepts_only_bare_hosts() {
    for valid in [
        "localhost",
        "api.example.com",
        "127.0.0.1",
        "my_service.internal",
    ] {
        assert!(is_valid_target_host(valid), "{valid} is a valid target");
    }
    for invalid in [
        "",
        "https://api.example.com",
        "api.example.com:8443",
        "api.example.com/path",
        "-leading.example",
        "trailing-.example",
        "spaced example.com",
    ] {
        assert!(!is_valid_target_host(invalid), "{invalid} is invalid");
    }
    assert!(!is_valid_target_host(&"a".repeat(254)));
}

#[test]
fn request_headers_default_to_no_passthrough() {
    let inbound = headers_of(&[("authorization", "secret"), ("x-tenant", "t1")]);
    let outbound = outbound_request_headers(&inbound, Some(&none_rules()), "api.example.com:443");
    assert_eq!(outbound.keys().collect::<Vec<_>>(), ["host"]);
    assert_eq!(outbound.get("host").unwrap(), "api.example.com:443");
}

#[test]
fn request_headers_allowlist_forwards_only_listed_names() {
    let inbound = headers_of(&[
        ("authorization", "secret"),
        ("x-tenant", "t1"),
        ("X-Tenant-Extra", "t2"),
    ]);
    let mut rules = none_rules();
    rules.passthrough = HeaderPassthrough::Allowlist;
    rules.passthrough_allowlist = vec![String::from("x-tenant"), String::from("X-Tenant-Extra")];
    let outbound = outbound_request_headers(&inbound, Some(&rules), "api.example.com:443");
    assert_eq!(outbound.get("x-tenant").unwrap(), "t1");
    assert_eq!(outbound.get("x-tenant-extra").unwrap(), "t2");
    assert!(!outbound.contains_key("authorization"));
}

#[test]
fn request_headers_forward_all_but_routing_headers_when_configured() {
    let inbound = headers_of(&[("x-tenant", "t1"), ("x-oagw-target-host", "host.internal")]);
    let mut rules = none_rules();
    rules.passthrough = HeaderPassthrough::All;
    let outbound = outbound_request_headers(&inbound, Some(&rules), "api.example.com:443");
    assert_eq!(outbound.get("x-tenant").unwrap(), "t1");
    assert!(!outbound.contains_key(TARGET_HOST_HEADER.as_str()));
}

#[test]
fn request_headers_apply_set_add_remove_rules() {
    let inbound = headers_of(&[("x-remove-me", "yes"), ("x-keep", "1")]);
    let mut rules = none_rules();
    rules.passthrough = HeaderPassthrough::All;
    rules.remove = vec![String::from("X-Remove-Me")];
    rules.set.insert(String::from("X-Set"), String::from("s"));
    rules.add.insert(String::from("X-Add"), String::from("a"));
    let outbound = outbound_request_headers(&inbound, Some(&rules), "api.example.com:443");
    assert!(!outbound.contains_key("x-remove-me"));
    assert_eq!(outbound.get("x-keep").unwrap(), "1");
    assert_eq!(outbound.get("x-set").unwrap(), "s");
    assert_eq!(outbound.get("x-add").unwrap(), "a");
}

#[test]
fn request_headers_set_the_upstream_authority_as_host() {
    let inbound = headers_of(&[("host", "gateway.internal")]);
    let outbound = outbound_request_headers(&inbound, None, "api.example.com:443");
    assert_eq!(outbound.get("host").unwrap(), "api.example.com:443");
}

#[test]
fn request_headers_strip_hop_by_hop_after_passthrough() {
    let inbound = headers_of(&[("connection", "x-doomed"), ("x-doomed", "yes")]);
    let mut rules = none_rules();
    rules.passthrough = HeaderPassthrough::All;
    let outbound = outbound_request_headers(&inbound, Some(&rules), "api.example.com:443");
    assert!(!outbound.contains_key("connection"));
    assert!(!outbound.contains_key("x-doomed"));
}

#[test]
fn request_headers_without_rules_forward_nothing() {
    let inbound = headers_of(&[("x-tenant", "t1")]);
    let outbound = outbound_request_headers(&inbound, None, "api.example.com:443");
    assert!(outbound.contains_key("host"));
    assert!(!outbound.contains_key("x-tenant"));
}

#[test]
fn response_headers_strip_hop_by_hop_and_content_length() {
    let upstream = headers_of(&[
        ("connection", "close"),
        ("content-length", "4"),
        ("x-kept", "yes"),
    ]);
    let outbound = outbound_response_headers(&upstream, None);
    assert!(!outbound.contains_key("connection"));
    assert!(!outbound.contains_key("content-length"));
    assert_eq!(outbound.get("x-kept").unwrap(), "yes");
}

#[test]
fn response_headers_apply_set_and_remove_rules() {
    let upstream = headers_of(&[("server", "nginx"), ("x-secret", "1")]);
    let mut rules = ResponseHeaderRules {
        remove: vec![String::from("X-Secret")],
        ..ResponseHeaderRules::default()
    };
    rules
        .set
        .insert(String::from("X-Frame-Options"), String::from("DENY"));
    let outbound = outbound_response_headers(&upstream, Some(&rules));
    assert!(!outbound.contains_key("x-secret"));
    assert_eq!(outbound.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(outbound.get("server").unwrap(), "nginx");
}

#[test]
fn filter_query_keeps_the_allowlisted_parameters() {
    let allowlist = vec![String::from("api-version"), String::from("top")];
    let filtered = filter_query(Some("?api-version=1.0&top=5"), &allowlist).unwrap();
    assert_eq!(filtered.as_deref(), Some("api-version=1.0&top=5"));
}

#[test]
fn filter_query_rejects_unknown_parameters() {
    let allowlist = vec![String::from("top")];
    let error = filter_query(Some("top=1&hidden=secret"), &allowlist).unwrap_err();
    match error {
        OagwError::Validation { detail } => {
            assert!(detail.contains("hidden"), "the offending name is reported");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn filter_query_allows_no_parameter_without_an_allowlist() {
    assert!(filter_query(Some("top=1"), &[]).is_err());
}

#[test]
fn filter_query_accepts_an_absent_query() {
    assert!(filter_query(None, &[]).unwrap().is_none());
    assert!(filter_query(Some(""), &[]).unwrap().is_none());
}

#[test]
fn request_id_header_is_the_documented_name() {
    assert_eq!(REQUEST_ID_HEADER.as_str(), "x-request-id");
}

#[test]
fn methods_of_the_header_module_are_case_insensitive() {
    let mut inbound = HeaderMap::new();
    inbound.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("UPGRADE"),
    );
    inbound.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
    strip_hop_by_hop(&mut inbound);
    assert!(!inbound.contains_key("upgrade"));
}
