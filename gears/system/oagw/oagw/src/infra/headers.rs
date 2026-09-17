//! Header transformation for the data plane.
//!
//! Implements the three header categories from `DESIGN.md §3.1`
//! (Headers Transformation): routing headers are consumed, hop-by-hop
//! headers are always stripped, and passthrough headers follow the
//! configured `passthrough` mode.

use http::header::{HeaderMap, HeaderName, HeaderValue};

/// Headers consumed by OAGW routing and never forwarded upstream.
pub const ROUTING_HEADERS: &[&str] = &["x-oagw-target-host"];

/// Hop-by-hop headers stripped by default per RFC 9110 §7.6.1.
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Fields of an upgrade handshake that survive the hop-by-hop strip.
///
/// A proxy that tunnels an upgrade forwards `Upgrade` (RFC 9110 §7.8.1) plus
/// the `Sec-WebSocket-*` request fields (RFC 6455 §4.1); without them the
/// upstream computes `Sec-WebSocket-Accept` from a missing key and every
/// RFC 6455 client rejects the handshake.
const UPGRADE_TUNNEL_HEADERS: &[&str] = &["upgrade", "connection"];

/// Response-only WebSocket field, never re-sent by the client side.
const UPGRADE_RESPONSE_ONLY: &str = "sec-websocket-accept";

/// Capture the upgrade handshake fields before hop-by-hop stripping.
#[must_use]
pub fn capture_upgrade_headers(headers: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            UPGRADE_TUNNEL_HEADERS.contains(&name)
                || (name.starts_with("sec-websocket-") && name != UPGRADE_RESPONSE_ONLY)
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Re-apply the captured upgrade handshake fields to the outbound request.
pub fn restore_upgrade_headers(headers: &mut HeaderMap, captured: &[(HeaderName, HeaderValue)]) {
    for (name, value) in captured {
        headers.insert(name.clone(), value.clone());
    }
}

/// Strip hop-by-hop + routing headers in place.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // Any header named by the `Connection` header is hop-by-hop too; the list
    // has to be read before `connection` itself is removed below.
    let named: Vec<HeaderName> = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter_map(|token| HeaderName::from_bytes(token.as_bytes()).ok())
                .collect()
        })
        .unwrap_or_default();
    for name in &named {
        headers.remove(name);
    }
    for name in ROUTING_HEADERS.iter().chain(HOP_BY_HOP_HEADERS.iter()) {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    headers.remove(http::header::HOST);
    headers.remove(http::header::CONTENT_LENGTH);
}

/// Apply request-side `set` / `add` / `remove` rules.
pub fn apply_request_rules(
    headers: &mut HeaderMap,
    rules: &crate::domain::model::RequestHeaderRules,
) {
    for name in &rules.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

/// Filter inbound headers down to the configured passthrough set.
///
/// Routing and hop-by-hop headers are always excluded regardless of mode.
pub fn apply_passthrough(
    inbound: &HeaderMap,
    rules: &crate::domain::model::RequestHeaderRules,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    let include = |name: &HeaderName| -> bool {
        let lower = name.as_str();
        !ROUTING_HEADERS.contains(&lower) && !HOP_BY_HOP_HEADERS.contains(&lower)
    };
    match rules.passthrough {
        crate::domain::model::PassthroughMode::None => {}
        crate::domain::model::PassthroughMode::All => {
            for (name, value) in inbound {
                if include(name) {
                    out.append(name, value.clone());
                }
            }
        }
        crate::domain::model::PassthroughMode::Allowlist => {
            for allowed in &rules.passthrough_allowlist {
                if let Ok(name) = HeaderName::from_bytes(allowed.as_bytes()) {
                    if !include(&name) {
                        continue;
                    }
                    if let Some(values) = inbound.get_all(&name).iter().next() {
                        out.insert(name, values.clone());
                    }
                }
            }
        }
    }
    out
}

/// Apply response-side `set` / `add` / `remove` rules.
pub fn apply_response_rules(
    headers: &mut HeaderMap,
    rules: &crate::domain::model::ResponseHeaderRules,
) {
    for name in &rules.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        PassthroughMode, RequestHeaderRules, ResponseHeaderRules,
    };
    use std::collections::BTreeMap;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    #[test]
    fn strips_hop_by_hop_and_routing_headers() {
        let mut h = map(&[
            ("connection", "close"),
            ("transfer-encoding", "chunked"),
            ("x-oagw-target-host", "b.internal"),
            ("host", "gateway.internal"),
            ("authorization", "Bearer abc"),
            ("x-api-key", "secret"),
        ]);
        strip_hop_by_hop(&mut h);
        assert!(h.get("connection").is_none());
        assert!(h.get("transfer-encoding").is_none());
        assert!(h.get("x-oagw-target-host").is_none());
        assert!(h.get("host").is_none());
        assert_eq!(h.get("authorization").unwrap(), "Bearer abc");
    }

    #[test]
    fn upgrade_handshake_survives_the_hop_by_hop_strip() {
        // The pipeline the data plane runs: capture → strip → passthrough →
        // restore.
        let inbound = map(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-accept", "client-must-not-send-this"),
            ("host", "gateway.internal"),
            ("authorization", "Bearer abc"),
        ]);
        let captured = capture_upgrade_headers(&inbound);
        let mut stripped = inbound.clone();
        strip_hop_by_hop(&mut stripped);
        let mut outbound = apply_passthrough(&stripped, &RequestHeaderRules::default());
        restore_upgrade_headers(&mut outbound, &captured);

        assert_eq!(outbound.get("upgrade").unwrap(), "websocket");
        assert_eq!(outbound.get("connection").unwrap(), "Upgrade");
        assert_eq!(
            outbound.get("sec-websocket-key").unwrap(),
            "dGhlIHNhbXBsZSBub25jZQ=="
        );
        assert_eq!(outbound.get("sec-websocket-version").unwrap(), "13");
        // A response-only field is never captured for re-sending.
        assert!(captured
            .iter()
            .all(|(name, _)| name != "sec-websocket-accept"));
        // Everything else stays subject to the configured passthrough mode.
        assert!(outbound.get("authorization").is_none());
        assert!(outbound.get("host").is_none());
    }

    #[test]
    fn removes_headers_named_by_connection() {
        let mut h = map(&[("connection", "x-custom, keep-alive"), ("x-custom", "v")]);
        strip_hop_by_hop(&mut h);
        assert!(h.get("x-custom").is_none());
    }

    #[test]
    fn passthrough_none_forwards_nothing() {
        let inbound = map(&[("authorization", "Bearer abc")]);
        let rules = RequestHeaderRules::default();
        assert!(apply_passthrough(&inbound, &rules).is_empty());
    }

    #[test]
    fn passthrough_allowlist_filters() {
        let inbound = map(&[
            ("authorization", "Bearer abc"),
            ("x-trace", "1"),
            ("connection", "close"),
        ]);
        let rules = RequestHeaderRules {
            passthrough: PassthroughMode::Allowlist,
            passthrough_allowlist: vec![
                "authorization".to_owned(),
                "connection".to_owned(),
            ],
            ..RequestHeaderRules::default()
        };
        let out = apply_passthrough(&inbound, &rules);
        assert_eq!(out.len(), 1);
        assert!(out.get("authorization").is_some());
        assert!(out.get("connection").is_none());
    }

    #[test]
    fn passthrough_all_excludes_hop_by_hop() {
        let inbound = map(&[("x-a", "1"), ("upgrade", "websocket")]);
        let rules = RequestHeaderRules {
            passthrough: PassthroughMode::All,
            ..RequestHeaderRules::default()
        };
        let out = apply_passthrough(&inbound, &rules);
        assert!(out.get("x-a").is_some());
        assert!(out.get("upgrade").is_none());
    }

    #[test]
    fn set_overrides_and_add_appends() {
        let mut h = map(&[("x-a", "1")]);
        let mut set = BTreeMap::new();
        set.insert("x-a".to_owned(), "2".to_owned());
        let mut add = BTreeMap::new();
        add.insert("x-b".to_owned(), "3".to_owned());
        let rules = RequestHeaderRules {
            set,
            add,
            ..Default::default()
        };
        apply_request_rules(&mut h, &rules);
        assert_eq!(h.get("x-a").unwrap(), "2");
        assert_eq!(h.get("x-b").unwrap(), "3");
    }

    #[test]
    fn response_rules_remove_and_set() {
        let mut h = map(&[("server", "upstream/1.0"), ("x-internal", "secret")]);
        let mut set = BTreeMap::new();
        set.insert("server".to_owned(), "oagw".to_owned());
        let rules = ResponseHeaderRules {
            set,
            add: BTreeMap::new(),
            remove: vec!["x-internal".to_owned()],
        };
        apply_response_rules(&mut h, &rules);
        assert_eq!(h.get("server").unwrap(), "oagw");
        assert!(h.get("x-internal").is_none());
    }

    #[test]
    fn invalid_header_values_are_skipped() {
        let mut h = HeaderMap::new();
        let mut set = BTreeMap::new();
        set.insert("x-bad".to_owned(), "bad\nvalue".to_owned());
        let rules = RequestHeaderRules {
            set,
            ..Default::default()
        };
        apply_request_rules(&mut h, &rules);
        assert!(h.get("x-bad").is_none());
    }
}
