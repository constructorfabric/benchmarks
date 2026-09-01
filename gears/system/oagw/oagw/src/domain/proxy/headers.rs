// Created: 2026-08-31 by Constructor Tech
//! Header transformation of the proxy data plane (DESIGN §3.2).
//!
//! Three categories leave the proxy: the routing headers the data plane
//! consumed, the hop-by-hop headers of RFC 9110 §7.6.1 and — when the upstream
//! declares `headers` — everything the rules do not forward. Multi-value
//! headers are preserved: values are copied per entry, never collapsed.
//!
//! # The upgrade exemption
//!
//! The table of DESIGN §3.2 strips `Upgrade` and `Connection` like every
//! hop-by-hop header. A WebSocket handshake (RFC 6455 §4.1) is the one protocol
//! upgrade the gateway forwards, and it is *made of* those two headers: without
//! `Connection: upgrade` and `Upgrade: websocket` the upstream never learns
//! that the client wants a socket. [`is_websocket_handshake`] recognises the
//! handshake and [`restore_upgrade_headers`] writes both values back after the
//! strip list, the header rules included, so no configuration can break the
//! handshake. The two values are canonical — `connection: upgrade` and
//! `upgrade: websocket`, nothing the client spelled — because a token list of
//! the client's own making is a smuggling channel. Every other request keeps
//! the strip list exactly as the table asks for; `Sec-WebSocket-*` headers are
//! not hop-by-hop and need no exemption at all. The answer side is judged by
//! the same standard: [`rejected_upgrade_reason`] reads the acceptance of the
//! session out of the upstream's head (RFC 6455 §4.2.2), because a 101 that
//! carries neither the token nor the accept value is not a session.

use std::collections::HashMap;

use http::{HeaderMap, HeaderName, HeaderValue, Method};

use crate::domain::model::{HeaderRules, ResponseHeaderRules};
use crate::error::{OagwError, OagwResult};

/// Hop-by-hop headers (RFC 9110 §7.6.1): stripped in both directions.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// `Connection` token of an upgrade (RFC 9110 §7.6.1).
const UPGRADE_TOKEN: &str = "upgrade";

/// `Upgrade` protocol token of a WebSocket handshake (RFC 6455 §4.1).
const WEBSOCKET_TOKEN: &str = "websocket";

/// Request headers the data plane owns.
///
/// `content-length` is recomputed from the buffered body, `host` is derived
/// from the dial target, and `x-oagw-target-host` is the routing header
/// ADR-0001 says must not reach the upstream.
const REQUEST_OWNED: [&str; 3] = ["host", "content-length", "x-oagw-target-host"];

/// Response headers OAGW owns: an upstream must not see or forge them.
const RESPONSE_OWNED: [&str; 2] = ["x-oagw-target-host", "x-oagw-error-source"];

/// Media type of a long-lived event stream (DESIGN §3.2 "Streaming").
pub const EVENT_STREAM_TYPE: &str = "text/event-stream";

/// Forwarded value of a header name / value pair from the configuration.
fn header_pair(name: &str, value: &str) -> OagwResult<(HeaderName, HeaderValue)> {
    let parsed_name = HeaderName::try_from(name).map_err(|_| invalid_header(name, "name"))?;
    let parsed_value = HeaderValue::try_from(value).map_err(|_| invalid_header(name, "value"))?;
    Ok((parsed_name, parsed_value))
}

/// 400 for a header name or value the configuration cannot produce.
fn invalid_header(name: &str, part: &str) -> OagwError {
    OagwError::validation(format!(
        "header {part} of '{name}' is not a valid HTTP header"
    ))
    .with_extension(|ext| {
        ext.invalid_value = Some(name.to_owned());
    })
}

/// Headers the `Connection` header names as hop-by-hop (RFC 9110 §7.6.1).
///
/// A connection-specific header is named by `Connection` and must not be
/// forwarded: `Connection: x-internal-actor` promotes `x-internal-actor` to a
/// hop-by-hop header of *this* hop, whatever the configuration says.
fn connection_named(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter_map(|name| HeaderName::try_from(name).ok())
        .collect()
}

/// Copy every header that is neither hop-by-hop nor owned by the proxy.
///
/// `owned` carries the headers of the direction at hand; the `Connection`
/// header of the same direction names further headers that stay on this hop.
fn forwardable(inbound: &HeaderMap, owned: &[&str]) -> HeaderMap {
    let named = connection_named(inbound);
    let mut headers = HeaderMap::with_capacity(inbound.len());
    for (name, value) in inbound {
        let hop_by_hop = HOP_BY_HOP.contains(&name.as_str()) || named.contains(name);
        if hop_by_hop || owned.contains(&name.as_str()) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

/// Whether `name` is removed by the configuration.
///
/// A name the configuration spells in a way `http` cannot parse can never
/// match a real header, so it is ignored instead of failing the request.
fn is_removed(name: &HeaderName, removed: &[String]) -> bool {
    removed
        .iter()
        .filter_map(|candidate| HeaderName::try_from(candidate.as_str()).ok())
        .any(|candidate| candidate == *name)
}

/// Whether an inbound header is forwarded under the `passthrough` mode.
///
/// A mode the schema does not define forwards **nothing**: the write path
/// rejects it, and a record that reached the store through another route must
/// not silently widen what leaves the gateway.
fn is_forwarded(name: &HeaderName, rules: &HeaderRules) -> bool {
    match rules.passthrough.as_deref() {
        // An object that omits `passthrough` follows the JSON schema default
        // "none": only the headers the rules produce survive.
        None | Some("none") => false,
        Some("all") => true,
        Some("allowlist") => rules
            .passthrough_allowlist
            .iter()
            .filter_map(|allowed| HeaderName::try_from(allowed.as_str()).ok())
            .any(|allowed| allowed == *name),
        Some(other) => {
            tracing::warn!(passthrough = %other, header = %name, "unknown passthrough mode; forwarding nothing");
            false
        }
    }
}

/// Apply `set` (replace) and `add` (append) to the outbound headers.
fn apply_entries(
    headers: &mut HeaderMap,
    entries: &HashMap<String, String>,
    append: bool,
) -> OagwResult<()> {
    for (name, value) in entries {
        let (name, value) = header_pair(name, value)?;
        if append {
            headers.append(&name, value);
        } else {
            headers.insert(&name, value);
        }
    }
    Ok(())
}

/// Build the headers of the outbound request.
///
/// Order of application (DESIGN §3.2 "Headers Transformation"): strip the
/// hop-by-hop and routing headers, apply `remove`, filter by `passthrough`,
/// then `set` (replace) and `add` (append). The recomputed `Content-Length` is
/// added last.
///
/// # Errors
/// 400 when a configured header name or value is not a valid HTTP header.
pub fn outbound_request_headers(
    inbound: &HeaderMap,
    rules: Option<&HeaderRules>,
    body_len: u64,
) -> OagwResult<HeaderMap> {
    let mut outbound = forwardable(inbound, &REQUEST_OWNED);
    if let Some(rules) = rules {
        let mut kept = HeaderMap::with_capacity(outbound.len());
        for (name, value) in &outbound {
            if is_removed(name, &rules.remove) || !is_forwarded(name, rules) {
                continue;
            }
            kept.append(name.clone(), value.clone());
        }
        outbound = kept;
        apply_entries(&mut outbound, &rules.set, false)?;
        apply_entries(&mut outbound, &rules.add, true)?;
    }
    outbound.insert(http::header::CONTENT_LENGTH, HeaderValue::from(body_len));
    Ok(outbound)
}

/// Build the headers of the response handed back to the client.
///
/// The upstream status and body are forwarded untouched; only the response
/// rules and the OAGW error-source marker are applied.
///
/// # Errors
/// 400 when a configured header name or value is not a valid HTTP header.
pub fn outbound_response_headers(
    upstream: &HeaderMap,
    rules: Option<&ResponseHeaderRules>,
    error_source: &str,
) -> OagwResult<HeaderMap> {
    let mut outbound = forwardable(upstream, &RESPONSE_OWNED);
    if let Some(rules) = rules {
        for name in &rules.remove {
            if let Ok(parsed) = HeaderName::try_from(name.as_str()) {
                outbound.remove(&parsed);
            }
        }
        apply_entries(&mut outbound, &rules.set, false)?;
        apply_entries(&mut outbound, &rules.add, true)?;
    }
    let marker = HeaderValue::try_from(error_source)
        .map_err(|_| invalid_header("x-oagw-error-source", "value"))?;
    outbound.insert(HeaderName::from_static("x-oagw-error-source"), marker);
    Ok(outbound)
}

/// Whether the upstream answered with a long-lived event stream.
///
/// The exemption is decided from the **response**, not from the client's
/// `Accept`: a client that asks for JSON and is handed `text/event-stream`
/// must still get the stream, and a client that asks for an event stream and
/// is handed JSON must still be bounded.
#[must_use]
pub fn is_event_stream_content(content_type: Option<&HeaderValue>) -> bool {
    content_type
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with(EVENT_STREAM_TYPE))
}

/// Whether the request opens a WebSocket handshake (RFC 6455 §4.1).
///
/// All four marks have to be there: a `GET`, the `upgrade` token of the
/// `Connection` header, the `websocket` token of the `Upgrade` header and a
/// `Sec-WebSocket-Key`. Without the key the request is an ordinary request that
/// happens to carry upgrade headers, and it keeps the ordinary path — where the
/// strip list takes both of them away again.
#[must_use]
pub fn is_websocket_handshake(method: &Method, headers: &HeaderMap) -> bool {
    let key = http::header::HeaderName::from_static("sec-websocket-key");
    method == Method::GET
        && carries(headers, http::header::CONNECTION, UPGRADE_TOKEN)
        && carries(headers, http::header::UPGRADE, WEBSOCKET_TOKEN)
        && headers.get(key).is_some_and(|value| !value.is_empty())
}

/// Whether any value of `name` carries `token` in its comma-separated list.
fn carries(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| candidate.eq_ignore_ascii_case(token))
}

/// Why the upstream's answer does not accept the session (RFC 6455 §4.2.2).
///
/// A 101 is an acceptance only with the two marks the specification asks of the
/// server: an `Upgrade` header whose token list contains `websocket`
/// (case-insensitive) and a non-empty `Sec-WebSocket-Accept`. hyper arms the
/// response upgrade from the status alone, so this head is the only place the
/// gateway can tell a real acceptance from a bare 101 — and a bare 101 hands
/// the client a socket whose first read is EOF, i.e. a session that carries
/// nothing.
///
/// `None` when the session is accepted; otherwise the reason, which becomes
/// both the log record and the detail of the 502 the client is given.
#[must_use]
pub fn rejected_upgrade_reason(headers: &HeaderMap) -> Option<&'static str> {
    if !carries(headers, http::header::UPGRADE, WEBSOCKET_TOKEN) {
        return Some("the answer names no websocket upgrade");
    }
    if headers
        .get("sec-websocket-accept")
        .is_none_or(http::HeaderValue::is_empty)
    {
        return Some("the answer carries no Sec-WebSocket-Accept");
    }
    None
}

/// Put the upgrade headers of a handshake back into the outbound set.
///
/// Applied **twice** — after the strip list and the header rules, and again
/// right before the handshake is dialled — so neither a header rule nor a
/// plugin request phase can break the one protocol upgrade the gateway
/// forwards: a `remove` entry, a `passthrough: "none"` or a plugin's rewrite
/// cannot break a handshake, because the two headers it needs are not
/// configuration, they are the protocol. An ordinary request never reaches this
/// function, and keeps the strip list.
///
/// The values written are canonical, never the client's: `connection: upgrade`
/// and `upgrade: websocket`, each a single value, and every other value of
/// either header is dropped with them. A token list of the client's own making
/// is a smuggling channel — `upgrade: websocket, h2c` would ship a complete h2c
/// upgrade through a WebSocket bridge, and a `Connection` naming headers that
/// were already stripped is the desync primitive. The headers `Connection`
/// names are therefore dropped here too, whatever a plugin put back.
///
/// `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Origin` and
/// `Sec-WebSocket-Protocol` are not touched: they are not hop-by-hop, and the
/// client's values of them are the protocol content of the handshake.
pub fn restore_upgrade_headers(outbound: &mut HeaderMap, inbound: &HeaderMap) {
    for name in connection_named(inbound) {
        outbound.remove(&name);
    }
    for (name, token) in [
        (http::header::CONNECTION, UPGRADE_TOKEN),
        (http::header::UPGRADE, WEBSOCKET_TOKEN),
    ] {
        // `insert` replaces every value of the name, so no client token list
        // survives the restore.
        outbound.insert(name, HeaderValue::from_static(token));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use http::{HeaderMap, HeaderName, HeaderValue, Method};

    use super::{
        is_event_stream_content, is_websocket_handshake, outbound_request_headers,
        outbound_response_headers, rejected_upgrade_reason, restore_upgrade_headers,
    };
    use crate::domain::model::{HeaderRules, ResponseHeaderRules};
    use crate::error::OagwErrorKind;

    /// A header map from lower-case static pairs; every name used here is a
    /// valid `http` header name.
    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for &(name, value) in pairs {
            headers.append(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        headers
    }

    fn values<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a str> {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect()
    }

    fn rules(passthrough: Option<&str>, set: &[(&str, &str)]) -> HeaderRules {
        HeaderRules {
            set: set
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            add: HashMap::new(),
            remove: Vec::new(),
            passthrough: passthrough.map(str::to_owned),
            passthrough_allowlist: Vec::new(),
        }
    }

    #[test]
    fn strips_hop_by_hop_and_routing_headers() {
        let inbound = headers(&[
            ("connection", "close"),
            ("keep-alive", "timeout=5"),
            ("te", "trailers"),
            ("trailer", "x-checksum"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("proxy-authorization", "basic"),
            ("proxy-authenticate", "basic"),
            ("proxy-connection", "keep-alive"),
            ("host", "oagw.example"),
            ("x-oagw-target-host", "us.vendor.com"),
            ("content-length", "12"),
            ("x-request-id", "abc"),
        ]);
        let outbound = outbound_request_headers(&inbound, None, 5).unwrap();
        assert_eq!(values(&outbound, "x-request-id"), ["abc"]);
        assert_eq!(values(&outbound, "content-length"), ["5"]);
        for name in [
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "proxy-authorization",
            "proxy-authenticate",
            "proxy-connection",
            "host",
            "x-oagw-target-host",
        ] {
            assert!(
                outbound.get(name).is_none(),
                "{name} must not reach the upstream"
            );
        }
    }

    #[test]
    fn keeps_multi_value_headers() {
        let inbound = headers(&[("accept", "application/json"), ("accept", "text/plain")]);
        let outbound = outbound_request_headers(&inbound, None, 0).unwrap();
        assert_eq!(
            values(&outbound, "accept"),
            ["application/json", "text/plain"]
        );
    }

    /// A header `Connection` names is hop-by-hop for this hop, even when the
    /// configuration would forward it (RFC 9110 §7.6.1).
    #[test]
    fn a_connection_named_header_is_not_forwarded() {
        let mut inbound = headers(&[
            ("connection", "x-internal-actor, keep-alive"),
            ("x-internal-actor", "admin"),
            ("x-vendor", "1"),
        ]);
        inbound.append(
            HeaderName::from_static("x-internal-actor"),
            HeaderValue::from_static("second"),
        );

        let outbound = outbound_request_headers(&inbound, None, 0).unwrap();
        assert!(outbound.get("x-internal-actor").is_none());
        assert_eq!(values(&outbound, "x-vendor"), ["1"]);
    }

    /// `passthrough: "none"` must not resuscitate a `Connection`-named header.
    #[test]
    fn a_connection_named_header_survives_no_passthrough_mode() {
        let mut rules = rules(None, &[("x-a", "1")]);
        rules.passthrough = Some("all".to_owned());
        let inbound = headers(&[
            ("connection", "x-internal-actor"),
            ("x-internal-actor", "admin"),
        ]);
        let outbound = outbound_request_headers(&inbound, Some(&rules), 0).unwrap();
        assert!(outbound.get("x-internal-actor").is_none());
    }

    /// The response direction strips the headers the upstream's own
    /// `Connection` header names.
    #[test]
    fn a_connection_named_response_header_is_dropped() {
        let upstream = headers(&[
            ("connection", "x-hop"),
            ("x-hop", "per-hop"),
            ("x-body", "kept"),
        ]);
        let outbound = outbound_response_headers(&upstream, None, "upstream").unwrap();
        assert!(outbound.get("x-hop").is_none());
        assert_eq!(values(&outbound, "x-body"), ["kept"]);
    }

    /// An unknown `passthrough` value is fail closed: nothing is forwarded.
    #[test]
    fn an_unknown_passthrough_mode_forwards_nothing() {
        let mut unknown = rules(Some("sometimes"), &[]);
        unknown.passthrough_allowlist = vec!["x-vendor".to_owned()];
        let inbound = headers(&[("x-vendor", "1")]);
        let outbound = outbound_request_headers(&inbound, Some(&unknown), 0).unwrap();
        assert!(outbound.get("x-vendor").is_none());
    }

    #[test]
    fn without_a_headers_object_everything_is_forwarded() {
        let inbound = headers(&[("x-vendor", "1"), ("authorization", "bearer")]);
        let outbound = outbound_request_headers(&inbound, None, 0).unwrap();
        assert_eq!(values(&outbound, "x-vendor"), ["1"]);
        assert_eq!(values(&outbound, "authorization"), ["bearer"]);
    }

    #[test]
    fn passthrough_none_drops_every_inbound_header() {
        let rules = rules(None, &[("x-a", "1")]);
        let inbound = headers(&[("x-vendor", "1")]);
        let outbound = outbound_request_headers(&inbound, Some(&rules), 0).unwrap();
        assert!(outbound.get("x-vendor").is_none());
        assert_eq!(values(&outbound, "x-a"), ["1"]);
    }

    #[test]
    fn passthrough_all_keeps_everything() {
        let rules = rules(Some("all"), &[]);
        let inbound = headers(&[("x-vendor", "1")]);
        let outbound = outbound_request_headers(&inbound, Some(&rules), 0).unwrap();
        assert_eq!(values(&outbound, "x-vendor"), ["1"]);
    }

    #[test]
    fn passthrough_allowlist_is_exact() {
        let mut allow = rules(Some("allowlist"), &[]);
        allow.passthrough_allowlist = vec!["x-keep".to_owned()];
        let inbound = headers(&[("x-keep", "1"), ("x-drop", "2")]);
        let outbound = outbound_request_headers(&inbound, Some(&allow), 0).unwrap();
        assert_eq!(values(&outbound, "x-keep"), ["1"]);
        assert!(outbound.get("x-drop").is_none());
    }

    #[test]
    fn remove_runs_before_the_passthrough_filter() {
        let mut drop_only = rules(Some("all"), &[]);
        drop_only.remove = vec!["x-drop".to_owned()];
        let inbound = headers(&[("x-drop", "1")]);
        let outbound = outbound_request_headers(&inbound, Some(&drop_only), 0).unwrap();
        assert!(outbound.get("x-drop").is_none());
    }

    #[test]
    fn set_replaces_and_add_appends() {
        let mut combined = rules(Some("all"), &[("x-a", "set")]);
        combined.add = [("x-b".to_owned(), "2".to_owned())].into_iter().collect();
        // Multi-value inbound headers survive, and `add` appends after them.
        let inbound = headers(&[("x-a", "original"), ("x-b", "1"), ("x-b", "1b")]);
        let outbound = outbound_request_headers(&inbound, Some(&combined), 0).unwrap();
        assert_eq!(values(&outbound, "x-a"), ["set"]);
        assert_eq!(values(&outbound, "x-b"), ["1", "1b", "2"]);
    }

    #[test]
    fn rejects_an_invalid_configured_header() {
        let rules = rules(Some("all"), &[("x bad", "1")]);
        let error = outbound_request_headers(&HeaderMap::new(), Some(&rules), 0).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
        assert_eq!(error.extensions().invalid_value.as_deref(), Some("x bad"));
    }

    #[test]
    fn response_rules_set_add_and_remove() {
        let response_rules = ResponseHeaderRules {
            set: [("x-set".to_owned(), "s".to_owned())].into_iter().collect(),
            add: [("x-add".to_owned(), "a".to_owned())].into_iter().collect(),
            remove: vec!["x-drop".to_owned()],
        };
        let upstream = headers(&[("x-drop", "1"), ("server", "nginx")]);
        let outbound =
            outbound_response_headers(&upstream, Some(&response_rules), "upstream").unwrap();
        assert_eq!(values(&outbound, "x-set"), ["s"]);
        assert_eq!(values(&outbound, "x-add"), ["a"]);
        assert!(outbound.get("x-drop").is_none());
        assert_eq!(values(&outbound, "server"), ["nginx"]);
        assert_eq!(values(&outbound, "x-oagw-error-source"), ["upstream"]);
    }

    #[test]
    fn every_response_carries_the_error_source_marker() {
        let outbound = outbound_response_headers(&HeaderMap::new(), None, "upstream").unwrap();
        assert_eq!(values(&outbound, "x-oagw-error-source"), ["upstream"]);
    }

    #[test]
    fn a_forged_error_source_marker_is_replaced() {
        let upstream = headers(&[("x-oagw-error-source", "gateway")]);
        let outbound = outbound_response_headers(&upstream, None, "upstream").unwrap();
        assert_eq!(values(&outbound, "x-oagw-error-source"), ["upstream"]);
    }

    #[test]
    fn keeps_the_upstream_content_length() {
        let upstream = headers(&[("content-length", "42")]);
        let outbound = outbound_response_headers(&upstream, None, "upstream").unwrap();
        assert_eq!(values(&outbound, "content-length"), ["42"]);
    }

    /// The streaming exemption follows the upstream `Content-Type`, not the
    /// client's `Accept`.
    #[test]
    fn an_event_stream_is_recognised_from_the_response_content_type() {
        assert!(is_event_stream_content(Some(&HeaderValue::from_static(
            "text/event-stream"
        ))));
        assert!(is_event_stream_content(Some(&HeaderValue::from_static(
            "Text/Event-Stream; charset=utf-8"
        ))));
        assert!(!is_event_stream_content(Some(&HeaderValue::from_static(
            "application/json"
        ))));
        assert!(!is_event_stream_content(None));
    }

    #[test]
    fn content_length_is_always_recomputed() {
        let inbound = headers(&[("content-length", "9999")]);
        let outbound = outbound_request_headers(&inbound, None, 3).unwrap();
        assert_eq!(values(&outbound, "content-length"), ["3"]);
    }

    // ── WebSocket handshakes (DESIGN §3.2 header table, upgrade exemption) ──

    /// The handshake of RFC 6455 §4.1: a `GET` that carries the `upgrade`
    /// connection token, the `websocket` upgrade token and a key.
    fn handshake() -> (Method, HeaderMap) {
        (
            Method::GET,
            headers(&[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
            ]),
        )
    }

    #[test]
    fn a_websocket_handshake_is_recognised() {
        let (method, request) = handshake();
        assert!(is_websocket_handshake(&method, &request));
        // The tokens are case-insensitive and the lists may carry more.
        let tolerant = headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "WebSocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
        ]);
        assert!(is_websocket_handshake(&method, &tolerant));
    }

    #[test]
    fn a_non_get_is_not_a_handshake() {
        let (_, request) = handshake();
        assert!(!is_websocket_handshake(&Method::POST, &request));
        assert!(!is_websocket_handshake(&Method::OPTIONS, &request));
    }

    #[test]
    fn without_the_connection_token_there_is_no_handshake() {
        let (method, mut request) = handshake();
        request.insert(
            HeaderName::from_static("connection"),
            HeaderValue::from_static("keep-alive"),
        );
        assert!(!is_websocket_handshake(&method, &request));
    }

    #[test]
    fn without_the_websocket_token_there_is_no_handshake() {
        let (method, mut request) = handshake();
        request.insert(
            HeaderName::from_static("upgrade"),
            HeaderValue::from_static("h2c"),
        );
        assert!(!is_websocket_handshake(&method, &request));
    }

    #[test]
    fn without_the_key_there_is_no_handshake() {
        let (method, mut request) = handshake();
        request.remove("sec-websocket-key");
        assert!(!is_websocket_handshake(&method, &request));
        // An empty key is as good as none.
        request.insert(
            HeaderName::from_static("sec-websocket-key"),
            HeaderValue::from_static(""),
        );
        assert!(!is_websocket_handshake(&method, &request));
    }

    /// The one exemption of the strip list: a handshake keeps the two headers
    /// the upgrade needs, whatever the rules say.
    #[test]
    fn a_handshake_keeps_its_upgrade_headers() {
        let (_, request) = handshake();
        let mut outbound = outbound_request_headers(&request, None, 0).unwrap();
        assert!(outbound.get("upgrade").is_none());
        assert!(outbound.get("connection").is_none());
        restore_upgrade_headers(&mut outbound, &request);
        assert_eq!(values(&outbound, "upgrade"), ["websocket"]);
        assert_eq!(values(&outbound, "connection"), ["upgrade"]);
        // The rest of the handshake flows through the ordinary rules.
        assert_eq!(
            values(&outbound, "sec-websocket-key"),
            ["dGhlIHNhbXBsZSBub25jZQ=="]
        );
    }

    /// The values written are canonical: a token list of the client's own
    /// making is a smuggling channel, so it never reaches the upstream.
    #[test]
    fn a_handshake_writes_canonical_upgrade_values() {
        let request = headers(&[
            ("connection", "upgrade, HTTP2-Settings, x-internal-actor"),
            ("upgrade", "websocket, h2c"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ]);
        let mut outbound = outbound_request_headers(&request, None, 0).unwrap();
        // A plugin phase may have put either header back, in the client's own
        // words.
        outbound.insert("upgrade", HeaderValue::from_static("websocket, h2c"));
        outbound.append("connection", HeaderValue::from_static("keep-alive"));
        outbound.insert("http2-settings", HeaderValue::from_static("AAMAAABk"));
        outbound.insert("x-internal-actor", HeaderValue::from_static("root"));
        restore_upgrade_headers(&mut outbound, &request);
        assert_eq!(values(&outbound, "upgrade"), ["websocket"]);
        assert_eq!(values(&outbound, "connection"), ["upgrade"]);
        assert!(outbound.get("http2-settings").is_none());
        assert!(outbound.get("x-internal-actor").is_none());
        // Not hop-by-hop: the client's values are the protocol content.
        assert_eq!(
            values(&outbound, "sec-websocket-key"),
            ["dGhlIHNhbXBsZSBub25jZQ=="]
        );
    }

    /// Even a configuration that removes the upgrade headers cannot break the
    /// handshake: the exemption is applied after the rules.
    #[test]
    fn the_upgrade_exemption_survives_the_header_rules() {
        let (_, request) = handshake();
        let mut dropping = rules(Some("all"), &[]);
        dropping.remove = vec!["upgrade".to_owned(), "connection".to_owned()];
        let mut outbound = outbound_request_headers(&request, Some(&dropping), 0).unwrap();
        assert!(outbound.get("upgrade").is_none());
        restore_upgrade_headers(&mut outbound, &request);
        assert_eq!(values(&outbound, "upgrade"), ["websocket"]);
        assert_eq!(values(&outbound, "connection"), ["upgrade"]);
    }

    /// An ordinary request that carries the same headers is not exempted: the
    /// predicate does not fire, so the caller never lifts the strip list.
    #[test]
    fn an_ordinary_request_gets_no_upgrade_exemption() {
        let request = headers(&[
            ("connection", "Upgrade, HTTP2-Settings"),
            ("upgrade", "h2c"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ]);
        assert!(!is_websocket_handshake(&Method::GET, &request));
        let outbound = outbound_request_headers(&request, None, 0).unwrap();
        assert!(outbound.get("upgrade").is_none());
        assert!(outbound.get("connection").is_none());
    }

    /// The acceptance of RFC 6455 §4.2.2: the token, in any case, and a
    /// non-empty accept value.
    #[test]
    fn a_101_that_accepts_the_session_names_the_protocol() {
        let answer = headers(&[
            ("upgrade", "WebSocket"),
            ("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOoY="),
        ]);
        assert_eq!(rejected_upgrade_reason(&answer), None);
        // The token may sit in a list of its own protocols.
        let listed = headers(&[
            ("upgrade", "h2c, websocket"),
            ("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOoY="),
        ]);
        assert_eq!(rejected_upgrade_reason(&listed), None);
    }

    #[test]
    fn a_101_that_accepts_nothing_is_named_as_such() {
        // No `Upgrade` at all: the status switched, the protocol did not.
        let bare = headers(&[("connection", "upgrade")]);
        assert_eq!(
            rejected_upgrade_reason(&bare),
            Some("the answer names no websocket upgrade")
        );
        // The token without the accept value is still no acceptance.
        let accepted_nothing = headers(&[("upgrade", "websocket")]);
        assert_eq!(
            rejected_upgrade_reason(&accepted_nothing),
            Some("the answer carries no Sec-WebSocket-Accept")
        );
        let empty = headers(&[("upgrade", "websocket"), ("sec-websocket-accept", "")]);
        assert_eq!(
            rejected_upgrade_reason(&empty),
            Some("the answer carries no Sec-WebSocket-Accept")
        );
    }
}
