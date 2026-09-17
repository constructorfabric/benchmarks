//! Header and WebSocket-upgrade transformation rules (DESIGN "Headers
//! Transformation", "Proxy API").
//!
//! Covers the pure helpers the data plane applies to proxied traffic:
//! hop-by-hop stripping, the `X-OAGW-Target-Host` routing header,
//! `X-Forwarded-*` synthesis, host rewrite and the `101 Switching Protocols`
//! handshake.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use http::{HeaderMap, HeaderValue};
use oagw::domain::model::{
    Endpoint, EndpointScheme, HeaderPassthrough, RequestHeaderRules, ResponseHeaderRules,
};
use oagw::infra::proxy::headers::{
    add_forwarding_headers, apply_request_rules, apply_response_rules, insert_lossy,
    is_hop_by_hop, strip_gateway_headers, HOP_BY_HOP_HEADERS, TARGET_HOST_HEADER,
};
use oagw::infra::proxy::upgrade::{
    switch_to_websocket, upstream_upgrade_headers, validate_target_host, websocket_accept,
    SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL,
};

fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn request_rules(
    set: &[(&str, &str)],
    add: &[(&str, &str)],
    remove: &[&str],
    passthrough: HeaderPassthrough,
    allowlist: &[&str],
) -> RequestHeaderRules {
    RequestHeaderRules {
        set: map(set),
        add: map(add),
        remove: remove.iter().map(|name| (*name).to_owned()).collect(),
        passthrough,
        passthrough_allowlist: allowlist.iter().map(|name| (*name).to_owned()).collect(),
    }
}

fn headers(entries: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut outbound = HeaderMap::new();
    for (name, value) in entries {
        outbound.insert(*name, HeaderValue::from_static(value));
    }
    outbound
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port,
    }
}

// ---------------------------------------------------------------------------
// Hop-by-hop stripping
// ---------------------------------------------------------------------------

#[test]
fn every_documented_hop_by_hop_header_is_stripped_by_default() {
    let mut inbound = headers(&[("x-end-to-end", "kept")]);
    for name in HOP_BY_HOP_HEADERS {
        inbound.insert(*name, HeaderValue::from_static("dropped"));
    }
    let outbound = apply_request_rules(&inbound, None);
    for name in HOP_BY_HOP_HEADERS {
        assert!(outbound.get(*name).is_none(), "{name} must not cross the proxy");
    }
    assert_eq!(outbound.get("x-end-to-end").unwrap(), "kept");
}

#[test]
fn hop_by_hop_detection_is_case_insensitive_and_covers_proxies() {
    for name in HOP_BY_HOP_HEADERS {
        assert!(is_hop_by_hop(&name.to_ascii_uppercase()), "{name}");
    }
    assert!(is_hop_by_hop("Proxy-Connection"));
    assert!(is_hop_by_hop("PROXY-AUTHORIZATION"));
    assert!(!is_hop_by_hop("x-request-id"));
    assert!(!is_hop_by_hop("authorization"));
}

#[test]
fn allowlist_forwards_only_the_listed_headers_with_every_value() {
    let mut inbound = headers(&[("x-kept", "one"), ("x-dropped", "no")]);
    inbound.append("x-kept", HeaderValue::from_static("second"));
    let rules = request_rules(
        &[],
        &[],
        &[],
        HeaderPassthrough::Allowlist,
        &["x-kept", "x-unknown"],
    );
    let outbound = apply_request_rules(&inbound, Some(&rules));
    let kept: Vec<&HeaderValue> = outbound.get_all("x-kept").iter().collect();
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0], "one");
    assert_eq!(kept[1], "second");
    assert!(outbound.get("x-dropped").is_none());
    assert!(outbound.get("x-unknown").is_none(), "allowlist names that the caller did not send contribute nothing");
}

#[test]
fn remove_wins_over_the_passthrough_mode_but_does_not_widen_it() {
    let mut inbound = headers(&[("x-secret", "no"), ("x-kept", "yes")]);
    inbound.append("x-kept", HeaderValue::from_static("also"));
    for passthrough in [
        HeaderPassthrough::None,
        HeaderPassthrough::Allowlist,
        HeaderPassthrough::All,
    ] {
        let allowlist = ["x-kept", "x-secret"];
        let rules = request_rules(&[], &[], &["x-kept"], passthrough, &allowlist);
        let outbound = apply_request_rules(&inbound, Some(&rules));
        assert!(outbound.get("x-kept").is_none(), "{passthrough:?}");
    }

    // `remove` cannot widen a restrictive mode: `None` still forwards nothing.
    let rules = request_rules(&[], &[], &["x-kept"], HeaderPassthrough::None, &["x-secret"]);
    let outbound = apply_request_rules(&inbound, Some(&rules));
    assert!(outbound.get("x-secret").is_none());
    assert!(outbound.is_empty());

    // `All` forwards everything it was going to forward anyway.
    let rules = request_rules(&[], &[], &["x-kept"], HeaderPassthrough::All, &[]);
    let outbound = apply_request_rules(&inbound, Some(&rules));
    assert_eq!(outbound.get("x-secret").unwrap(), "no");
}

#[test]
fn set_overwrites_while_add_appends_a_second_value() {
    let inbound = headers(&[("x-set", "old"), ("x-add", "first")]);
    let rules = request_rules(
        &[("x-set", "new")],
        &[("x-add", "second")],
        &[],
        HeaderPassthrough::All,
        &[],
    );
    let outbound = apply_request_rules(&inbound, Some(&rules));
    assert_eq!(outbound.get("x-set").unwrap(), "new");
    let added: Vec<&HeaderValue> = outbound.get_all("x-add").iter().collect();
    assert_eq!(added.len(), 2);
    assert_eq!(added[0], "first");
    assert_eq!(added[1], "second");
}

#[test]
fn default_passthrough_forwards_nothing_when_rules_are_present() {
    // `HeaderPassthrough::None` is the default, so a rules object that only
    // names `set`/`add` turns the upstream request into exactly those headers.
    let inbound = headers(&[("x-secret", "must-not-leak")]);
    let rules = request_rules(
        &[("x-traceparent", "00-trace")],
        &[],
        &[],
        HeaderPassthrough::None,
        &[],
    );
    let outbound = apply_request_rules(&inbound, Some(&rules));
    assert!(outbound.get("x-secret").is_none());
    assert_eq!(outbound.get("x-traceparent").unwrap(), "00-trace");
}

#[test]
fn malformed_rule_entries_are_dropped_instead_of_panicking() {
    let inbound = headers(&[("x-kept", "yes")]);
    let rules = request_rules(
        &[("x-bad value", "ok"), ("x-valid", "ok")],
        &[("x-bad\nvalue", "ok"), ("x-added", "ok")],
        &["x-kept"],
        HeaderPassthrough::None,
        &[],
    );
    let outbound = apply_request_rules(&inbound, Some(&rules));
    assert!(outbound.get("x-bad value").is_none());
    assert!(outbound.get("x-valid").is_some());
    assert!(outbound.get("x-added").is_some());
}

#[test]
fn response_rules_strip_hop_by_hop_and_apply_their_own_rules() {
    let upstream = headers(&[("content-type", "application/json"), ("connection", "close")]);
    let rules = ResponseHeaderRules {
        set: map(&[("x-upstream", "oagw")]),
        add: map(&[("x-added", "1")]),
        remove: vec!["content-type".to_owned()],
    };
    let outbound = apply_response_rules(&upstream, Some(&rules), true);
    assert!(outbound.get("content-type").is_none());
    assert!(outbound.get("connection").is_none());
    assert_eq!(outbound.get("x-upstream").unwrap(), "oagw");
    assert_eq!(outbound.get("x-added").unwrap(), "1");
}

#[test]
fn response_rules_without_configuration_only_strip_hop_by_hop() {
    let upstream = headers(&[("content-type", "application/json"), ("keep-alive", "timeout=5")]);
    let outbound = apply_response_rules(&upstream, None, false);
    assert_eq!(outbound.get("content-type").unwrap(), "application/json");
    assert!(outbound.get("keep-alive").is_none());
}

// ---------------------------------------------------------------------------
// Routing headers and forwarding headers
// ---------------------------------------------------------------------------

#[test]
fn gateway_routing_headers_are_removed_before_forwarding() {
    let mut inbound = headers(&[
        ("x-oagw-target-host", "b.example.com"),
        ("host", "gateway.example"),
        ("connection", "keep-alive"),
        ("x-api-key", "client-secret"),
    ]);
    strip_gateway_headers(&mut inbound);
    assert!(inbound.get(TARGET_HOST_HEADER).is_none());
    assert!(inbound.get("host").is_none());
    assert!(inbound.get("connection").is_none());
    assert_eq!(inbound.get("x-api-key").unwrap(), "client-secret");
}

#[test]
fn forwarding_headers_declare_the_client_host_and_https() {
    let mut outbound = headers(&[("x-api-key", "value")]);
    add_forwarding_headers(&mut outbound, Some("client.example"));
    assert_eq!(outbound.get("x-forwarded-host").unwrap(), "client.example");
    assert_eq!(outbound.get("x-forwarded-proto").unwrap(), "https");
}

#[test]
fn forwarding_headers_omit_the_host_when_the_client_sent_none() {
    let mut outbound = HeaderMap::new();
    add_forwarding_headers(&mut outbound, None);
    assert!(outbound.get("x-forwarded-host").is_none());
    assert_eq!(outbound.get("x-forwarded-proto").unwrap(), "https");
}

#[test]
fn insert_lossy_ignores_values_that_are_not_valid_header_values() {
    let mut outbound = HeaderMap::new();
    insert_lossy(&mut outbound, "x-fine", "ok");
    insert_lossy(&mut outbound, "x-\ninvalid", "ok");
    insert_lossy(&mut outbound, "x-untouched", "bad\u{0}value");
    assert_eq!(outbound.get("x-fine").unwrap(), "ok");
    assert_eq!(outbound.len(), 1);
}

// ---------------------------------------------------------------------------
// WebSocket upgrade
// ---------------------------------------------------------------------------

#[test]
fn switch_to_websocket_answers_the_client_handshake() {
    let client = headers(&[(
        SEC_WEBSOCKET_KEY,
        "dGhlIHNhbXBsZSBub25jZQ==",
    )]);
    let response = switch_to_websocket(&client, Some("chat")).unwrap();
    assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(response.headers().get("upgrade").unwrap(), "websocket");
    assert_eq!(response.headers().get("connection").unwrap(), "Upgrade");
    assert_eq!(
        response.headers().get("sec-websocket-accept").unwrap(),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
    assert_eq!(
        response.headers().get(SEC_WEBSOCKET_PROTOCOL).unwrap(),
        "chat"
    );
}

#[test]
fn switch_to_websocket_without_a_negotiated_protocol_omits_it() {
    let client = headers(&[(SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")]);
    let response = switch_to_websocket(&client, None).unwrap();
    assert!(response.headers().get(SEC_WEBSOCKET_PROTOCOL).is_none());
}

#[test]
fn switch_to_websocket_requires_a_client_key() {
    let response = switch_to_websocket(&HeaderMap::new(), None);
    assert!(matches!(
        response.unwrap_err(),
        oagw::domain::error::OagwError::Validation(_)
    ));
}

#[test]
fn accept_is_derived_from_the_trimmed_key() {
    assert_eq!(
        websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
        websocket_accept("\tdGhlIHNhbXBsZSBub25jZQ==\n")
    );
}

#[test]
fn upstream_upgrade_headers_carry_the_websocket_negotiation_and_host() {
    let client = headers(&[
        (SEC_WEBSOCKET_KEY, "client-key"),
        ("sec-websocket-version", "13"),
        (SEC_WEBSOCKET_PROTOCOL, "chat"),
        ("sec-websocket-extensions", "permessage-deflate"),
        ("x-irrelevant", "no"),
    ]);
    let outbound = upstream_upgrade_headers(&client, "upstream.example:8443");
    assert_eq!(outbound.get(SEC_WEBSOCKET_KEY).unwrap(), "client-key");
    assert_eq!(outbound.get("sec-websocket-version").unwrap(), "13");
    assert_eq!(outbound.get(SEC_WEBSOCKET_PROTOCOL).unwrap(), "chat");
    assert_eq!(
        outbound.get("sec-websocket-extensions").unwrap(),
        "permessage-deflate"
    );
    assert_eq!(outbound.get("host").unwrap(), "upstream.example:8443");
    assert!(outbound.get("x-irrelevant").is_none());
    assert_eq!(outbound.len(), 5);
}

// ---------------------------------------------------------------------------
// X-OAGW-Target-Host validation
// ---------------------------------------------------------------------------

#[test]
fn target_host_pinning_matches_host_with_or_without_port() {
    let endpoints = vec![endpoint("A.Example.com", 8443), endpoint("b.example.com", 443)];
    assert_eq!(validate_target_host(&endpoints, Some("a.example.com")).unwrap(), Some(0));
    assert_eq!(
        validate_target_host(&endpoints, Some("a.example.com:8443")).unwrap(),
        Some(0)
    );
    assert_eq!(
        validate_target_host(&endpoints, Some("a.example.com.")).unwrap(),
        Some(0),
        "a trailing dot is normalized away"
    );
    assert_eq!(
        validate_target_host(&endpoints, Some(" b.example.com ")).unwrap(),
        Some(1)
    );
}

#[test]
fn target_host_matching_takes_the_port_from_the_endpoint_not_the_header() {
    let endpoints = vec![endpoint("a.example.com", 8443)];
    // A host match wins over the port spelled in the header: the caller cannot
    // steer traffic to a different port than the configured endpoint's.
    assert_eq!(
        validate_target_host(&endpoints, Some("a.example.com:443")).unwrap(),
        Some(0)
    );
    assert_eq!(validate_target_host(&endpoints, Some("A.Example.com")).unwrap(), Some(0));

    // A host that is not configured is unknown, with or without a port.
    for requested in ["other.example.com", "other.example.com:8443"] {
        assert!(
            matches!(
                validate_target_host(&endpoints, Some(requested)),
                Err(oagw::domain::error::OagwError::UnknownTargetHost)
            ),
            "{requested}"
        );
    }
}

#[test]
fn target_host_rejects_paths_and_orphan_ports() {
    let endpoints = vec![endpoint("a.example.com", 443)];
    for requested in ["a.example.com/path", ":8443", "a.example.com:", "-bad-"] {
        assert!(
            matches!(
                validate_target_host(&endpoints, Some(requested)),
                Err(oagw::domain::error::OagwError::InvalidTargetHost)
            ),
            "{requested}"
        );
    }
}
