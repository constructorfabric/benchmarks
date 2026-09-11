//! Header transformation.
//!
//! Hop-by-hop headers are always stripped; the routing headers
//! `X-OAGW-Target-Host` and `Host` are consumed by the gateway and never
//! forwarded. `set`, `add`, `remove` and the passthrough modes then apply.

use std::collections::BTreeMap;

use crate::domain::dto::{HeaderPassthrough, RequestHeaderRules, ResponseHeaderRules};

/// Headers that never cross a proxy hop.
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The target-host selector, read during routing then stripped.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Headers the gateway owns and never forwards.
pub const GATEWAY_OWNED: [&str; 2] = ["host", TARGET_HOST_HEADER];

/// Headers a WebSocket handshake cannot be negotiated without.
///
/// They are hop-by-hop, and so stripped from an ordinary proxied request, but
/// the upgrade *is* the hop: a tunnel that drops them never reaches 101.
pub const WEBSOCKET_HANDSHAKE: [&str; 7] = [
    "connection",
    "host",
    "upgrade",
    "sec-websocket-extensions",
    "sec-websocket-key",
    "sec-websocket-protocol",
    "sec-websocket-version",
];

/// Whether a header name is stripped from any forwarded message.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lowered.as_str())
}

/// Whether a header name is consumed by the gateway during routing.
#[must_use]
pub fn is_gateway_owned(name: &str) -> bool {
    name.eq_ignore_ascii_case(TARGET_HOST_HEADER) || name.eq_ignore_ascii_case("host")
}

/// Removes the hop-by-hop and gateway-owned headers from `headers`.
pub fn strip_unforwardable(headers: &mut http::HeaderMap) {
    let doomed: Vec<http::HeaderName> = headers
        .keys()
        .filter(|name| is_hop_by_hop(name.as_str()) || is_gateway_owned(name.as_str()))
        .cloned()
        .collect();
    for name in doomed {
        headers.remove(&name);
    }
}

/// Applies `set`, `add` and `remove` rules to a header map.
pub fn apply_rules(
    headers: &mut http::HeaderMap,
    set: &BTreeMap<String, String>,
    add: &BTreeMap<String, String>,
    remove: &[String],
) {
    for name in remove {
        if let Ok(header) = http::HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(&header);
        }
    }
    for (name, value) in set {
        if let (Ok(header), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(header, value);
        }
    }
    for (name, value) in add {
        if let (Ok(header), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.append(header, value);
        }
    }
}

/// Builds the outbound request headers for a WebSocket upgrade.
///
/// Identical to [`outbound_request_headers`] except that the handshake headers
/// survive whatever the passthrough rule says: without `upgrade`,
/// `connection` and the `sec-websocket-*` pair the upstream cannot accept the
/// upgrade, so the tunnel would end before it began.
#[must_use]
pub fn websocket_request_headers(
    inbound: &http::HeaderMap,
    rules: Option<&RequestHeaderRules>,
    extra: &[(String, String)],
) -> http::HeaderMap {
    let mut outbound = outbound_request_headers(inbound, rules, extra);
    for name in WEBSOCKET_HANDSHAKE {
        let Ok(header) = http::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        if let Some(value) = inbound.get(&header) {
            outbound.insert(header, value.clone());
        }
    }
    outbound
}

/// Builds the outbound request headers from the inbound set, the rules and any
/// gateway-injected extras. With `passthrough: none` (the default) no inbound
/// header survives; the outbound set is exactly what the rules produce.
#[must_use]
pub fn outbound_request_headers(
    inbound: &http::HeaderMap,
    rules: Option<&RequestHeaderRules>,
    extra: &[(String, String)],
) -> http::HeaderMap {
    let rules = rules.cloned().unwrap_or_default();
    let mut outbound = http::HeaderMap::new();
    for (name, value) in inbound {
        if is_hop_by_hop(name.as_str()) || is_gateway_owned(name.as_str()) {
            continue;
        }
        let allowed = match rules.passthrough {
            HeaderPassthrough::None => false,
            HeaderPassthrough::All => true,
            HeaderPassthrough::Allowlist => rules
                .passthrough_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name.as_str())),
        };
        if allowed {
            outbound.append(name.clone(), value.clone());
        }
    }
    apply_rules(&mut outbound, &rules.set, &rules.add, &rules.remove);
    for (name, value) in extra {
        if let (Ok(header), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            outbound.insert(header, value);
        }
    }
    outbound
}

/// Builds the client-facing response headers.
#[must_use]
pub fn response_headers(
    upstream: &http::HeaderMap,
    rules: Option<&ResponseHeaderRules>,
    extra: &[(String, String)],
) -> http::HeaderMap {
    let rules = rules.cloned().unwrap_or_default();
    let mut outbound = http::HeaderMap::new();
    for (name, value) in upstream {
        if is_hop_by_hop(name.as_str()) || is_gateway_owned(name.as_str()) {
            continue;
        }
        outbound.append(name.clone(), value.clone());
    }
    apply_rules(&mut outbound, &rules.set, &rules.add, &rules.remove);
    for (name, value) in extra {
        if let (Ok(header), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            outbound.insert(header, value);
        }
    }
    outbound
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn inbound() -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in [
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("host", "gateway.test"),
            ("x-caller-header", "kept-by-no-one"),
        ] {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn the_default_passthrough_forwards_nothing() {
        let outbound = outbound_request_headers(&inbound(), None, &[]);
        assert!(outbound.is_empty());
    }

    #[test]
    fn a_websocket_handshake_survives_the_default_passthrough() {
        let outbound = websocket_request_headers(&inbound(), None, &[]);
        assert_eq!(outbound.get("upgrade").unwrap(), "websocket");
        assert_eq!(outbound.get("sec-websocket-key").unwrap(), "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(outbound.get("sec-websocket-version").unwrap(), "13");
        // Everything else still obeys the rules.
        assert!(outbound.get("x-caller-header").is_none());
    }

    #[test]
    fn a_websocket_handshake_does_not_invent_missing_headers() {
        let mut map = http::HeaderMap::new();
        map.insert(http::header::UPGRADE, http::HeaderValue::from_static("websocket"));
        let outbound = websocket_request_headers(&map, None, &[]);
        assert_eq!(outbound.get("upgrade").unwrap(), "websocket");
        assert!(outbound.get("sec-websocket-key").is_none());
    }

    #[test]
    fn an_explicit_allowlist_still_applies_to_ordinary_headers() {
        let rules = RequestHeaderRules {
            passthrough: HeaderPassthrough::Allowlist,
            passthrough_allowlist: vec!["x-caller-header".to_owned()],
            ..RequestHeaderRules::default()
        };
        let outbound = outbound_request_headers(&inbound(), Some(&rules), &[]);
        assert_eq!(outbound.get("x-caller-header").unwrap(), "kept-by-no-one");
        assert!(outbound.get("upgrade").is_none());
    }
}
