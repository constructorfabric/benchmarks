//! Header transformation between the inbound proxy request and the outbound
//! upstream request (`cpt-cf-oagw-fr-header-transform`).

use http::header::{HeaderName, HeaderValue};
use http::HeaderMap;

use crate::domain::model::{PassthroughMode, RequestHeaderRules, ResponseHeaderRules};
use crate::util::{is_hop_by_hop, is_routing_header};

/// Entity headers that describe the body being forwarded.
///
/// `DESIGN.md` requires well-known headers to be "validated, set or adjusted";
/// they travel with the payload rather than being subject to the general
/// inbound passthrough policy, otherwise the default `passthrough: none` would
/// silently strip the media type off every proxied POST.
const ALWAYS_FORWARDED: &[&str] = &[
    "content-type",
    "content-encoding",
    "content-language",
    "accept",
];

/// Build the outbound header set from the inbound one.
///
/// Order of operations: passthrough selection → `remove` → `set` → `add`.
/// Hop-by-hop and OAGW routing headers are dropped unconditionally, and
/// `Host` is always replaced by the upstream authority.
#[must_use]
pub fn build_outbound(
    inbound: &HeaderMap,
    rules: &RequestHeaderRules,
    upstream_authority: &str,
) -> HeaderMap {
    let mut out = HeaderMap::new();

    for (name, value) in inbound {
        let key = name.as_str();
        if is_hop_by_hop(key) || is_routing_header(key) {
            continue;
        }
        if key.eq_ignore_ascii_case("host") || key.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let forced = ALWAYS_FORWARDED.iter().any(|h| key.eq_ignore_ascii_case(h));
        let selected = match rules.passthrough {
            PassthroughMode::All => true,
            PassthroughMode::None => false,
            PassthroughMode::Allowlist => rules
                .passthrough_allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(key)),
        };
        if forced || selected {
            out.append(name.clone(), value.clone());
        }
    }

    for name in &rules.remove {
        if let Ok(header) = HeaderName::try_from(name.as_str()) {
            out.remove(&header);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(header), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            out.insert(header, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(header), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            out.append(header, value);
        }
    }

    if let Ok(value) = HeaderValue::from_str(upstream_authority) {
        out.insert(http::header::HOST, value);
    }
    out
}

/// Copy the upstream response headers, dropping hop-by-hop members and
/// applying the configured response rules.
#[must_use]
pub fn build_response(upstream: &HeaderMap, rules: &ResponseHeaderRules) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    for name in &rules.remove {
        if let Ok(header) = HeaderName::try_from(name.as_str()) {
            out.remove(&header);
        }
    }
    for (name, value) in &rules.set {
        if let (Ok(header), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            out.insert(header, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(header), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            out.append(header, value);
        }
    }
    out
}

/// Headers forwarded verbatim during an upgrade handshake.
///
/// The `Connection` / `Upgrade` pair and the `Sec-WebSocket-*` negotiation
/// headers are hop-by-hop for a normal proxy but are exactly what has to reach
/// the upstream here.
#[must_use]
pub fn build_upgrade_headers(inbound: &HeaderMap, outbound: &HeaderMap) -> HeaderMap {
    let mut out = outbound.clone();
    for name in [
        http::header::CONNECTION,
        http::header::UPGRADE,
        http::header::SEC_WEBSOCKET_KEY,
        http::header::SEC_WEBSOCKET_VERSION,
        http::header::SEC_WEBSOCKET_PROTOCOL,
        http::header::SEC_WEBSOCKET_EXTENSIONS,
    ] {
        out.remove(&name);
        for value in inbound.get_all(&name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn inbound() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(http::header::HOST, HeaderValue::from_static("oagw.local"));
        h.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        h.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer inbound-token"),
        );
        h.insert(http::header::CONNECTION, HeaderValue::from_static("close"));
        h.insert("x-oagw-target-host", HeaderValue::from_static("us.vendor.com"));
        h.insert("x-custom", HeaderValue::from_static("keep-me"));
        h
    }

    #[test]
    fn default_passthrough_none_keeps_only_entity_headers() {
        let rules = RequestHeaderRules::default();
        let out = build_outbound(&inbound(), &rules, "api.openai.com");
        assert_eq!(
            out.get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        // The caller's bearer token is not leaked to the upstream by default.
        assert!(out.get(http::header::AUTHORIZATION).is_none());
        assert!(out.get("x-custom").is_none());
    }

    #[test]
    fn hop_by_hop_and_routing_headers_are_always_stripped() {
        let mut rules = RequestHeaderRules::default();
        rules.passthrough = PassthroughMode::All;
        let out = build_outbound(&inbound(), &rules, "api.openai.com");
        assert!(out.get(http::header::CONNECTION).is_none());
        assert!(out.get("x-oagw-target-host").is_none());
        assert_eq!(
            out.get(http::header::HOST).and_then(|v| v.to_str().ok()),
            Some("api.openai.com")
        );
        assert_eq!(
            out.get("x-custom").and_then(|v| v.to_str().ok()),
            Some("keep-me")
        );
    }

    #[test]
    fn allowlist_selects_named_headers_only() {
        let mut rules = RequestHeaderRules::default();
        rules.passthrough = PassthroughMode::Allowlist;
        rules.passthrough_allowlist = vec!["X-Custom".to_owned()];
        let out = build_outbound(&inbound(), &rules, "api.openai.com");
        assert!(out.get("x-custom").is_some());
        assert!(out.get(http::header::AUTHORIZATION).is_none());
    }

    #[test]
    fn set_add_and_remove_apply_in_order() {
        let mut rules = RequestHeaderRules::default();
        rules.passthrough = PassthroughMode::All;
        rules.remove = vec!["x-custom".to_owned()];
        let mut set = BTreeMap::new();
        set.insert("x-api-version".to_owned(), "2026-01".to_owned());
        rules.set = set;
        let mut add = BTreeMap::new();
        add.insert("x-trace".to_owned(), "on".to_owned());
        rules.add = add;

        let out = build_outbound(&inbound(), &rules, "api.openai.com");
        assert!(out.get("x-custom").is_none());
        assert_eq!(
            out.get("x-api-version").and_then(|v| v.to_str().ok()),
            Some("2026-01")
        );
        assert_eq!(out.get("x-trace").and_then(|v| v.to_str().ok()), Some("on"));
    }

    #[test]
    fn response_rules_strip_hop_by_hop_and_apply_edits() {
        let mut upstream = HeaderMap::new();
        upstream.insert(
            http::header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        upstream.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain"),
        );
        upstream.insert("x-internal", HeaderValue::from_static("secret"));

        let mut rules = ResponseHeaderRules::default();
        rules.remove = vec!["x-internal".to_owned()];
        let mut set = BTreeMap::new();
        set.insert("cache-control".to_owned(), "no-store".to_owned());
        rules.set = set;

        let out = build_response(&upstream, &rules);
        assert!(out.get(http::header::TRANSFER_ENCODING).is_none());
        assert!(out.get("x-internal").is_none());
        assert_eq!(
            out.get(http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
    }

    #[test]
    fn upgrade_headers_restore_the_handshake_pair() {
        let mut inbound = HeaderMap::new();
        inbound.insert(http::header::CONNECTION, HeaderValue::from_static("Upgrade"));
        inbound.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        inbound.insert(
            http::header::SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        inbound.insert(
            http::header::SEC_WEBSOCKET_VERSION,
            HeaderValue::from_static("13"),
        );

        let outbound = HeaderMap::new();
        let out = build_upgrade_headers(&inbound, &outbound);
        assert_eq!(
            out.get(http::header::UPGRADE).and_then(|v| v.to_str().ok()),
            Some("websocket")
        );
        assert_eq!(
            out.get(http::header::SEC_WEBSOCKET_KEY)
                .and_then(|v| v.to_str().ok()),
            Some("dGhlIHNhbXBsZSBub25jZQ==")
        );
    }
}
