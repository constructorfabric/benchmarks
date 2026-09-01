#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

//! `DESIGN` §"Headers Transformation" — what crosses the data plane.

use std::collections::{BTreeMap, BTreeSet};

use super::headers::{
    HOP_BY_HOP, HOST_HEADER, ROUTING_HEADERS, is_forwarded, is_upgrade, outbound_request_headers,
    outbound_response_headers,
};
use crate::domain::model::{Passthrough, RequestHeaderRules, ResponseHeaderRules};

fn inbound(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn names(headers: &http::HeaderMap) -> BTreeSet<String> {
    headers
        .keys()
        .map(|name| name.as_str().to_owned())
        .collect()
}

fn values(headers: &http::HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect()
}

// ── categories ─────────────────────────────────────────────────────────────

#[test]
fn hop_by_hop_routing_and_framing_headers_are_not_forwarded() {
    let mut refused = BTreeSet::new();
    for name in HOP_BY_HOP.iter().chain(ROUTING_HEADERS) {
        refused.insert((*name).to_owned());
    }
    refused.insert("content-length".to_owned());

    for name in refused {
        assert!(
            !is_forwarded(&name),
            "'{name}' belongs to one side of the gateway"
        );
    }
    for name in ["authorization", "accept", "x-request-id", "user-agent"] {
        assert!(is_forwarded(name), "'{name}' is an end-to-end header");
    }
    // Case and padding do not rescue a refused header.
    assert!(!is_forwarded("  Transfer-Encoding "));
}

#[test]
fn an_upgrade_request_is_detected_from_both_headers() {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONNECTION,
        "keep-alive, Upgrade".parse().unwrap(),
    );
    headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
    assert!(is_upgrade(&headers));

    // Without the `Connection` token the `Upgrade` header alone is not one.
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
    assert!(!is_upgrade(&headers));

    // And without a protocol name the token alone is not one either.
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::CONNECTION, "upgrade".parse().unwrap());
    assert!(!is_upgrade(&headers));
}

// ── outbound request ───────────────────────────────────────────────────────

#[test]
fn the_host_header_is_always_the_endpoint_authority() {
    let outbound = outbound_request_headers(
        &inbound(&[("host", "caller.example"), ("accept", "application/json")]),
        "api.openai.com",
        None,
        false,
        &BTreeSet::new(),
    );
    assert_eq!(values(&outbound, HOST_HEADER), vec!["api.openai.com"]);
    assert_eq!(values(&outbound, "accept"), vec!["application/json"]);
}

#[test]
fn the_caller_headers_are_forwarded_when_the_upstream_configures_nothing() {
    let outbound = outbound_request_headers(
        &inbound(&[
            ("authorization", "Bearer tok"),
            ("x-oagw-target-host", "eu.vendor.com"),
            ("x-request-id", "r-1"),
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("content-length", "12"),
            ("x-oagw-error-source", "gateway"),
        ]),
        "api.openai.com",
        None,
        false,
        &BTreeSet::new(),
    );
    // End-to-end headers are forwarded; `Host` is rewritten, and the headers
    // the gateway consumed to route the call never travel upstream.
    assert_eq!(
        names(&outbound),
        BTreeSet::from([
            "authorization".to_owned(),
            "host".to_owned(),
            "x-request-id".to_owned()
        ])
    );
    assert_eq!(values(&outbound, "authorization"), vec!["Bearer tok"]);
    assert_eq!(values(&outbound, HOST_HEADER), vec!["api.openai.com"]);
    assert_eq!(
        values(&outbound, "x-oagw-target-host"),
        Vec::<String>::new()
    );
    assert_eq!(
        values(&outbound, "x-oagw-error-source"),
        Vec::<String>::new()
    );
}

#[test]
fn a_whitelisted_upstream_forwards_only_the_names_it_lists() {
    let rules = RequestHeaderRules {
        set: BTreeMap::new(),
        add: BTreeMap::new(),
        remove: Vec::new(),
        passthrough: Some(Passthrough::Allowlist),
        passthrough_allowlist: vec!["Authorization".to_owned(), "x-trace".to_owned()],
    };
    let outbound = outbound_request_headers(
        &inbound(&[
            ("authorization", "Bearer tok"),
            ("x-trace", "t"),
            ("x-secret", "nope"),
        ]),
        "api.openai.com",
        Some(&rules),
        false,
        &BTreeSet::new(),
    );
    assert_eq!(values(&outbound, "authorization"), vec!["Bearer tok"]);
    assert_eq!(values(&outbound, "x-trace"), vec!["t"]);
    assert_eq!(values(&outbound, "x-secret"), Vec::<String>::new());
    assert_eq!(values(&outbound, HOST_HEADER), vec!["api.openai.com"]);
}

#[test]
fn an_upstream_that_asks_for_no_header_forwards_none() {
    let rules = RequestHeaderRules {
        set: BTreeMap::new(),
        add: BTreeMap::new(),
        remove: Vec::new(),
        passthrough: Some(Passthrough::None),
        passthrough_allowlist: Vec::new(),
    };
    let outbound = outbound_request_headers(
        &inbound(&[("authorization", "Bearer tok"), ("accept", "*/*")]),
        "api.openai.com",
        Some(&rules),
        false,
        &BTreeSet::new(),
    );
    assert_eq!(names(&outbound), BTreeSet::from(["host".to_owned()]));
}

#[test]
fn set_and_add_are_applied_after_the_passthrough_filter() {
    let mut set = BTreeMap::new();
    set.insert("X-Served-By".to_owned(), "oagw".to_owned());
    let mut add = BTreeMap::new();
    add.insert("x-vendor-key".to_owned(), "vk".to_owned());
    let rules = RequestHeaderRules {
        set,
        add,
        remove: vec!["x-caller-secret".to_owned()],
        passthrough: Some(Passthrough::All),
        passthrough_allowlist: Vec::new(),
    };
    let outbound = outbound_request_headers(
        &inbound(&[
            ("x-caller-secret", "s"),
            ("x-vendor-key", "leaked"),
            ("x-served-by", "other"),
        ]),
        "api.openai.com",
        Some(&rules),
        false,
        &BTreeSet::new(),
    );
    assert_eq!(values(&outbound, "x-caller-secret"), Vec::<String>::new());
    assert_eq!(values(&outbound, "x-served-by"), vec!["oagw"]);
    assert_eq!(
        values(&outbound, "x-vendor-key"),
        vec!["leaked", "vk"],
        "`add` appends, it does not overwrite"
    );
}

#[test]
fn an_upgrade_keeps_its_handshake_headers() {
    let outbound = outbound_request_headers(
        &inbound(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
        ]),
        "ws.example:8080",
        None,
        true,
        &BTreeSet::new(),
    );
    assert_eq!(values(&outbound, "connection"), vec!["Upgrade"]);
    assert_eq!(values(&outbound, "upgrade"), vec!["websocket"]);
    assert_eq!(
        values(&outbound, "sec-websocket-key"),
        vec!["dGhlIHNhbXBsZSBub25jZQ=="]
    );
}

#[test]
fn a_header_the_http_grammar_refuses_is_dropped() {
    let outbound = outbound_request_headers(
        &inbound(&[("x-bad", "bad\u{7f}value"), ("x-good", "ok")]),
        "api.openai.com",
        None,
        false,
        &BTreeSet::new(),
    );
    assert_eq!(values(&outbound, "x-bad"), Vec::<String>::new());
    assert_eq!(values(&outbound, "x-good"), vec!["ok"]);
}

// ── outbound response ──────────────────────────────────────────────────────

#[test]
fn the_response_drops_its_framing_and_keeps_the_rest() {
    let mut upstream = http::HeaderMap::new();
    upstream.insert("content-length", "12".parse().unwrap());
    upstream.insert("transfer-encoding", "chunked".parse().unwrap());
    upstream.insert("connection", "close".parse().unwrap());
    upstream.insert("x-request-id", "r".parse().unwrap());
    upstream.insert("server", "vendor".parse().unwrap());

    let outbound = outbound_response_headers(&upstream, None);
    assert_eq!(
        names(&outbound),
        BTreeSet::from(["x-request-id".to_owned(), "server".to_owned()])
    );
}

#[test]
fn response_rules_remove_set_and_add() {
    let mut set = BTreeMap::new();
    set.insert("Server".to_owned(), "oagw".to_owned());
    let mut add = BTreeMap::new();
    add.insert("x-oagw-proxied".to_owned(), "true".to_owned());
    let rules = ResponseHeaderRules {
        set,
        add,
        remove: vec!["x-vendor-internal".to_owned()],
    };

    let mut upstream = http::HeaderMap::new();
    upstream.insert("server", "vendor".parse().unwrap());
    upstream.insert("x-vendor-internal", "v".parse().unwrap());

    let outbound = outbound_response_headers(&upstream, Some(&rules));
    assert_eq!(values(&outbound, "server"), vec!["oagw"]);
    assert_eq!(values(&outbound, "x-vendor-internal"), Vec::<String>::new());
    assert_eq!(values(&outbound, "x-oagw-proxied"), vec!["true"]);
}

#[test]
fn a_multiplicity_the_upstream_sent_is_kept() {
    let mut upstream = http::HeaderMap::new();
    upstream.append("set-cookie", "a=1".parse().unwrap());
    upstream.append("set-cookie", "b=2".parse().unwrap());

    let outbound = outbound_response_headers(&upstream, None);
    assert_eq!(
        values(&outbound, "set-cookie"),
        vec!["a=1".to_owned(), "b=2".to_owned()]
    );
}

#[test]
fn an_unusable_response_header_is_dropped_rather_than_failing_the_call() {
    let mut upstream = http::HeaderMap::new();
    upstream.append("x-oagw-target-host", "hidden".parse().unwrap());
    upstream.append("server", "vendor".parse().unwrap());

    let outbound = outbound_response_headers(&upstream, None);
    assert_eq!(names(&outbound), BTreeSet::from(["server".to_owned()]));
}
