//! Unit tests for [`super::headers`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use super::{
    build_outbound_request_headers, build_outbound_response_headers, describe_rules,
    is_hop_by_hop, request_passthrough_mode, rule_header_names, HOP_BY_HOP_HEADERS,
    TARGET_HOST_HEADER,
};
use crate::domain::models::{HeaderPassthrough, HeaderRules};

fn rules() -> HeaderRules {
    HeaderRules {
        set: BTreeMap::new(),
        add: BTreeMap::new(),
        remove: Vec::new(),
        passthrough: None,
        passthrough_allowlist: Vec::new(),
    }
}

fn set(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn inbound() -> Vec<(String, String)> {
    vec![
        ("host".to_owned(), "gateway.internal".to_owned()),
        ("content-type".to_owned(), "application/json".to_owned()),
        ("authorization".to_owned(), "Bearer caller".to_owned()),
        ("x-request-id".to_owned(), "abc".to_owned()),
        ("x-oagw-target-host".to_owned(), "us.vendor.com".to_owned()),
        ("connection".to_owned(), "keep-alive".to_owned()),
    ]
}

#[test]
fn hop_by_hop_headers_are_listed_per_the_design_table() {
    for name in ["connection", "transfer-encoding", "upgrade", "te"] {
        assert!(is_hop_by_hop(name), "{name} must be stripped");
    }
    assert_eq!(HOP_BY_HOP_HEADERS.len(), 8);
    assert_eq!(TARGET_HOST_HEADER, "x-oagw-target-host");
}

#[test]
fn an_absent_headers_block_is_a_transparent_proxy() {
    let outbound = build_outbound_request_headers(&inbound(), "api.example.com", None);
    assert!(outbound.contains(&("authorization".to_owned(), "Bearer caller".to_owned())));
    assert_eq!(
        header(&outbound, "host"),
        Some("api.example.com".to_owned())
    );
    assert!(header(&outbound, "connection").is_none());
    assert!(header(&outbound, "x-oagw-target-host").is_none());
}

#[test]
fn the_default_request_mode_forwards_no_inbound_header() {
    let outbound = build_outbound_request_headers(&inbound(), "api.example.com", Some(&rules()));
    assert_eq!(
        outbound,
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("host".to_owned(), "api.example.com".to_owned()),
        ]
    );
    assert_eq!(request_passthrough_mode(Some(&rules())), HeaderPassthrough::None);
}

#[test]
fn the_allowlist_mode_forwards_only_listed_headers() {
    let mut config = rules();
    config.passthrough = Some(HeaderPassthrough::Allowlist);
    config.passthrough_allowlist = vec!["X-Request-Id".to_owned()];
    let outbound = build_outbound_request_headers(&inbound(), "api.example.com", Some(&config));
    assert_eq!(
        outbound,
        vec![
            ("x-request-id".to_owned(), "abc".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("host".to_owned(), "api.example.com".to_owned()),
        ]
    );
}

#[test]
fn the_all_mode_strips_hop_by_hop_and_routing_headers() {
    let mut config = rules();
    config.passthrough = Some(HeaderPassthrough::All);
    let outbound = build_outbound_request_headers(&inbound(), "api.example.com", Some(&config));
    assert_eq!(
        header(&outbound, "authorization"),
        Some("Bearer caller".to_owned())
    );
    assert!(header(&outbound, "connection").is_none());
    assert!(header(&outbound, "x-oagw-target-host").is_none());
    assert!(header(&outbound, "host").is_some());
}

#[test]
fn set_add_and_remove_are_applied_in_order() {
    let mut config = rules();
    config.passthrough = Some(HeaderPassthrough::All);
    config.set = set(&[("x-oagw-tenant", "t1")]);
    config.add = set(&[("x-added", "1")]);
    config.remove = vec!["Authorization".to_owned()];
    let outbound = build_outbound_request_headers(&inbound(), "api.example.com", Some(&config));
    assert_eq!(header(&outbound, "x-oagw-tenant"), Some("t1".to_owned()));
    assert_eq!(header(&outbound, "x-added"), Some("1".to_owned()));
    assert_eq!(header(&outbound, "authorization"), None);
}

#[test]
fn set_replaces_an_existing_header() {
    let mut config = rules();
    config.passthrough = Some(HeaderPassthrough::All);
    config.set = set(&[("content-type", "text/plain")]);
    let outbound = build_outbound_request_headers(&inbound(), "api.example.com", Some(&config));
    assert_eq!(
        header(&outbound, "content-type"),
        Some("text/plain".to_owned())
    );
    assert_eq!(value_count(&outbound, "content-type"), 1);
}

#[test]
fn response_headers_strip_hop_by_hop_and_apply_rules() {
    let upstream_response = vec![
        ("Content-Type".to_owned(), "text/event-stream".to_owned()),
        ("Connection".to_owned(), "close".to_owned()),
        ("X-Internal-Trace".to_owned(), "secret".to_owned()),
        ("Transfer-Encoding".to_owned(), "chunked".to_owned()),
    ];
    let mut config = rules();
    config.remove = vec!["x-internal-trace".to_owned()];
    config.set = set(&[("x-served-by", "oagw")]);

    let outbound = build_outbound_response_headers(&upstream_response, Some(&config));
    assert_eq!(
        header(&outbound, "content-type"),
        Some("text/event-stream".to_owned())
    );
    assert!(header(&outbound, "connection").is_none());
    assert!(header(&outbound, "transfer-encoding").is_none());
    assert!(header(&outbound, "x-internal-trace").is_none());
    assert_eq!(header(&outbound, "x-served-by"), Some("oagw".to_owned()));
}

#[test]
fn response_rules_without_a_config_pass_everything_through() {
    let upstream_response = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Server".to_owned(), "envoy".to_owned()),
    ];
    let outbound = build_outbound_response_headers(&upstream_response, None);
    assert_eq!(outbound.len(), 2);
}

#[test]
fn rule_descriptions_never_leak_values() {
    let mut config = rules();
    config.set = set(&[("authorization", "Bearer super-secret")]);
    config.passthrough = Some(HeaderPassthrough::Allowlist);
    let description = describe_rules(Some(&config));
    assert!(!description.contains("secret"), "{description}");
    assert_eq!(rule_header_names(&config), vec!["authorization"]);
    assert_eq!(describe_rules(None), "none");
}

#[test]
fn empty_header_values_are_forwarded_verbatim() {
    let headers = vec![("x-empty".to_owned(), String::new())];
    let mut config = rules();
    config.passthrough = Some(HeaderPassthrough::All);
    let outbound = build_outbound_request_headers(&headers, "api.example.com", Some(&config));
    assert_eq!(header(&outbound, "x-empty"), Some(String::new()));
}

fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn value_count(headers: &[(String, String)], name: &str) -> usize {
    headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .count()
}
