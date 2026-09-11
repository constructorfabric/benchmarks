//! Tests for outbound header rewriting.

use http::HeaderMap;

use crate::domain::upstream::{HeaderOp, HeadersConfig, PassthroughMode, RequestHeaders};
use crate::proxy::headers::{
    build_request_headers, build_response_headers, is_dropped, remove, set,
};

fn text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn op(name: &str, value: &str) -> HeaderOp {
    HeaderOp {
        name: name.to_owned(),
        value: value.to_owned(),
    }
}

fn inbound(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    headers
}

fn config(passthrough: PassthroughMode) -> HeadersConfig {
    HeadersConfig {
        request: RequestHeaders {
            passthrough,
            ..RequestHeaders::default()
        },
        ..HeadersConfig::default()
    }
}

/// The caller's headers pass through untouched by default.
#[test]
fn every_caller_header_passes_through_by_default() {
    let inbound = inbound(&[("x-request-id", "r-1"), ("accept", "application/json")]);
    let out = build_request_headers(&inbound, &config(PassthroughMode::All), "up.example.com", &[]);
    assert_eq!(text(&out, "x-request-id"), Some("r-1"));
    assert_eq!(text(&out, "accept"), Some("application/json"));
}

/// The gateway's own routing headers never reach the upstream.
#[test]
fn routing_and_hop_by_hop_headers_are_dropped() {
    let inbound = inbound(&[
        ("x-oagw-target-host", "internal.example.com"),
        ("connection", "close"),
        ("transfer-encoding", "chunked"),
        ("upgrade", "websocket"),
        ("keep-alive", "timeout=5"),
        ("proxy-authorization", "Basic zzz"),
        ("x-trace", "kept"),
    ]);
    let out = build_request_headers(&inbound, &config(PassthroughMode::All), "up.example.com", &[]);
    for dropped in [
        "x-oagw-target-host",
        "connection",
        "transfer-encoding",
        "upgrade",
        "keep-alive",
        "proxy-authorization",
    ] {
        assert!(out.get(dropped).is_none(), "{dropped} was forwarded");
    }
    assert_eq!(text(&out, "x-trace"), Some("kept"));
}

#[test]
fn the_host_header_is_always_the_resolved_target() {
    let inbound = inbound(&[("host", "caller.example.com")]);
    for passthrough in [PassthroughMode::All, PassthroughMode::None, PassthroughMode::Allowlist] {
        let out = build_request_headers(
            &inbound,
            &config(passthrough),
            "upstream.example.com",
            &[],
        );
        assert_eq!(
            text(&out, "host"),
            Some("upstream.example.com"),
            "passthrough {passthrough:?}"
        );
    }
}

/// A credential injection wins over a static `set` rule and over the caller.
#[test]
fn an_injected_credential_overrides_a_static_rule() {
    let mut config = config(PassthroughMode::All);
    config.request.set.push(op("authorization", "Bearer static"));
    let out = build_request_headers(
        &inbound(&[("authorization", "Bearer caller")]),
        &config,
        "up.example.com",
        &[("authorization".to_owned(), "Bearer real".to_owned())],
    );
    assert_eq!(text(&out, "authorization"), Some("Bearer real"));
}

#[test]
fn set_overwrites_and_add_fills_an_empty_header() {
    let mut config = HeadersConfig::default();
    config.request.set.push(op("x-mode", "set-wins"));
    config.request.add.push(op("x-mode", "add-loses"));
    config.request.add.push(op("x-extra", "added"));
    let out = build_request_headers(
        &inbound(&[("x-mode", "caller")]),
        &config,
        "up.example.com",
        &[],
    );
    assert_eq!(text(&out, "x-mode"), Some("set-wins"));
    assert_eq!(text(&out, "x-extra"), Some("added"));
}

#[test]
fn remove_drops_a_header_from_the_forwarded_request() {
    let mut config = HeadersConfig::default();
    config.request.remove.push("x-internal-token".to_owned());
    let out = build_request_headers(
        &inbound(&[("x-internal-token", "s"), ("x-keep", "k")]),
        &config,
        "up.example.com",
        &[],
    );
    assert!(out.get("x-internal-token").is_none());
    assert_eq!(text(&out, "x-keep"), Some("k"));
}

#[test]
fn the_none_mode_forwards_no_caller_header() {
    let inbound = inbound(&[("x-a", "1"), ("x-b", "2")]);
    let mut config = config(PassthroughMode::None);
    config.request.set.push(op("x-static", "v"));
    let out = build_request_headers(&inbound, &config, "up.example.com", &[]);
    assert!(out.get("x-a").is_none());
    assert!(out.get("x-b").is_none());
    assert_eq!(text(&out, "x-static"), Some("v"));
    assert_eq!(text(&out, "host"), Some("up.example.com"));
}

#[test]
fn the_allowlist_mode_keeps_only_named_headers() {
    let mut config = config(PassthroughMode::Allowlist);
    config.request.passthrough_allowlist = vec!["x-keep".to_owned()];
    let out = build_request_headers(
        &inbound(&[("x-keep", "1"), ("x-drop", "2")]),
        &config,
        "up.example.com",
        &[],
    );
    assert_eq!(text(&out, "x-keep"), Some("1"));
    assert!(out.get("x-drop").is_none());
}

/// Hop-by-hop headers the upstream set are dropped from the relayed response, and the
/// response rules are applied.
#[test]
fn the_response_headers_are_filtered_and_rewritten() {
    let upstream = inbound(&[
        ("content-type", "application/json"),
        ("connection", "close"),
        ("server", "unit-test"),
    ]);
    let mut config = crate::domain::upstream::ResponseHeaders::default();
    config.add.push(op("x-gateway", "oagw"));
    config.set.push(op("server", "oagw"));
    config.remove.push("x-leak".to_owned());

    let out = build_response_headers(&upstream, &config, &[]);
    assert_eq!(text(&out, "content-type"), Some("application/json"));
    assert!(out.get("connection").is_none(), "hop-by-hop forwarded");
    assert_eq!(text(&out, "server"), Some("oagw"));
    assert_eq!(text(&out, "x-gateway"), Some("oagw"));

    let with_leak = inbound(&[("x-leak", "1")]);
    let out = build_response_headers(&with_leak, &config, &[]);
    assert!(out.get("x-leak").is_none(), "remove ignored");
}

/// The upstream is never told who the caller was beyond what it asks for.
#[test]
fn no_caller_credential_reaches_the_upstream_by_default() {
    let inbound = inbound(&[("authorization", "Bearer downstream")]);
    let out = build_request_headers(
        &inbound,
        &HeadersConfig::default(),
        "up.example.com",
        &[("authorization".to_owned(), "Bearer upstream".to_owned())],
    );
    assert_eq!(text(&out, "authorization"), Some("Bearer upstream"));
}

#[test]
fn the_drop_table_is_explicit() {
    assert!(is_dropped("host"));
    assert!(is_dropped("x-oagw-target-host"));
    assert!(is_dropped("te"));
    assert!(is_dropped("trailer"));
    assert!(!is_dropped("x-request-id"));
    assert!(!is_dropped("content-type"));
}

#[test]
fn set_and_remove_ignore_invalid_names() {
    let mut headers = HeaderMap::new();
    set(&mut headers, "x-ok", "v");
    set(&mut headers, "bad header", "v");
    set(&mut headers, "x-bad", "invalid\nvalue");
    assert_eq!(text(&headers, "x-ok"), Some("v"));
    assert!(headers.get("bad header").is_none());
    assert!(!headers.contains_key("x-bad"));

    remove(&mut headers, "x-ok");
    remove(&mut headers, "bad header");
    assert!(headers.get("x-ok").is_none());
}
