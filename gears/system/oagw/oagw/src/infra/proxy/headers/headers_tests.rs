//! Tests for header transformation and `X-OAGW-Target-Host` parsing.

use super::*;
use crate::domain::error::DomainError;
use crate::domain::model::{RequestHeaderRules, ResponseHeaderRules};

fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        map.insert(
            http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            http::HeaderValue::from_str(v).unwrap(),
        );
    }
    map
}

#[test]
fn hop_by_hop_headers_are_stripped() {
    let inbound = hm(&[
        ("Connection", "keep-alive"),
        ("Keep-Alive", "timeout=5"),
        ("Transfer-Encoding", "chunked"),
        ("Upgrade", "websocket"),
        ("X-Custom", "v"),
    ]);
    let out = strip_gateway_headers(&inbound);
    assert!(out.get("connection").is_none());
    assert!(out.get("keep-alive").is_none());
    assert!(out.get("transfer-encoding").is_none());
    assert!(out.get("upgrade").is_none());
    assert_eq!(out.get("x-custom").unwrap(), "v");
}

#[test]
fn target_host_header_is_stripped() {
    let inbound = hm(&[("X-OAGW-Target-Host", "us.vendor.com"), ("X-Custom", "v")]);
    let out = strip_gateway_headers(&inbound);
    assert!(out.get("x-oagw-target-host").is_none());
    assert_eq!(out.get("x-custom").unwrap(), "v");
}

#[test]
fn host_header_survives_stripping_and_is_replaced_later() {
    let inbound = hm(&[("Host", "gateway.local"), ("X-Custom", "v")]);
    let out = strip_gateway_headers(&inbound);
    assert_eq!(out.get("host").unwrap(), "gateway.local");
}

#[test]
fn passthrough_none_drops_application_headers() {
    let rules = RequestHeaderRules::default();
    let inbound = hm(&[("X-Secret", "s"), ("Content-Type", "application/json")]);
    let out = build_outbound_headers(&inbound, &rules);
    assert!(out.get("x-secret").is_none());
    // Structural headers survive so the body stays well-formed.
    assert_eq!(out.get("content-type").unwrap(), "application/json");
}

#[test]
fn passthrough_allowlist_forwards_only_listed_headers() {
    let rules = RequestHeaderRules {
        passthrough: PassthroughMode::Allowlist,
        passthrough_allowlist: vec!["X-Custom".to_owned()],
        ..Default::default()
    };
    let inbound = hm(&[
        ("X-Custom", "1"),
        ("X-Other", "2"),
        ("Accept", "text/plain"),
    ]);
    let out = build_outbound_headers(&inbound, &rules);
    assert_eq!(out.get("x-custom").unwrap(), "1");
    assert!(out.get("x-other").is_none());
}

#[test]
fn passthrough_all_forwards_everything() {
    let rules = RequestHeaderRules {
        passthrough: PassthroughMode::All,
        ..Default::default()
    };
    let inbound = hm(&[("X-Custom", "1"), ("Accept", "text/plain")]);
    let out = build_outbound_headers(&inbound, &rules);
    assert_eq!(out.get("x-custom").unwrap(), "1");
}

#[test]
fn set_overrides_and_add_appends() {
    let mut set = std::collections::BTreeMap::new();
    set.insert("x-injected".to_owned(), "yes".to_owned());
    let mut add = std::collections::BTreeMap::new();
    add.insert("x-multi".to_owned(), "a".to_owned());
    let rules = RequestHeaderRules {
        set,
        add,
        passthrough: PassthroughMode::All,
        ..Default::default()
    };
    let inbound = hm(&[("X-Injected", "no"), ("X-Multi", "0")]);
    let out = build_outbound_headers(&inbound, &rules);
    assert_eq!(out.get("x-injected").unwrap(), "yes");
    assert_eq!(out.get_all("x-multi").iter().count(), 2);
}

#[test]
fn add_appends_without_passthrough() {
    let mut add = std::collections::BTreeMap::new();
    add.insert("x-multi".to_owned(), "a".to_owned());
    let rules = RequestHeaderRules {
        add,
        ..Default::default()
    };
    let inbound = hm(&[("X-Multi", "0")]);
    let out = build_outbound_headers(&inbound, &rules);
    // `none` drops the inbound header, so only the added value survives.
    assert_eq!(out.get_all("x-multi").iter().count(), 1);
    assert_eq!(out.get("x-multi").unwrap(), "a");
}

#[test]
fn remove_drops_headers() {
    let rules = RequestHeaderRules {
        remove: vec!["x-drop-me".to_owned()],
        passthrough: PassthroughMode::All,
        ..Default::default()
    };
    let inbound = hm(&[("X-Drop-Me", "1"), ("X-Keep", "2")]);
    let out = build_outbound_headers(&inbound, &rules);
    assert!(out.get("x-drop-me").is_none());
    assert_eq!(out.get("x-keep").unwrap(), "2");
}

#[test]
fn response_rules_are_applied() {
    let mut set = std::collections::BTreeMap::new();
    set.insert("x-resp".to_owned(), "set".to_owned());
    let config = HeadersConfig {
        response: ResponseHeaderRules {
            set,
            add: std::collections::BTreeMap::new(),
            remove: vec!["x-upstream-only".to_owned()],
        },
        ..Default::default()
    };
    let mut headers = hm(&[("X-Upstream-Only", "1"), ("X-Other", "2")]);
    apply_response_headers(&mut headers, &config);
    assert!(headers.get("x-upstream-only").is_none());
    assert_eq!(headers.get("x-resp").unwrap(), "set");
    assert_eq!(headers.get("x-other").unwrap(), "2");
}

#[test]
fn target_host_parsing() {
    assert_eq!(parse_target_host("us.vendor.com").unwrap(), "us.vendor.com");
    assert_eq!(
        parse_target_host("  US.Vendor.COM. ").unwrap(),
        "us.vendor.com"
    );
    assert_eq!(parse_target_host("10.0.0.1").unwrap(), "10.0.0.1");

    for bad in [
        "us.vendor.com:8443",
        "http://us.vendor.com",
        "us.vendor.com/path",
        "",
    ] {
        let err = parse_target_host(bad).unwrap_err();
        assert!(
            matches!(err, DomainError::InvalidTargetHost(_)),
            "`{bad}` should be invalid"
        );
    }
}
