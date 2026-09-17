//! Request/response header handling for the data plane.
//!
//! Two independent concerns live here:
//!
//! 1. *Hop-by-hop hygiene* (DESIGN "Hop-by-hop headers"): `Connection` and
//!    everything it names, plus the RFC 9110 list, never cross the proxy. The
//!    gateway's own routing headers (`X-OAGW-Target-Host`,
//!    `X-OAGW-Error-Source`, `X-OAGW-Trace-Id`) are likewise never forwarded
//!    upstream.
//! 2. *Operator header rules* (`upstream.headers` / `route.headers`), applied
//!    in a fixed order — `remove`, `set`, `add` — so the semantics are
//!    deterministic regardless of map iteration order.
//!
//! Header transformation runs *after* the auth and transform plugin phases,
//! so configured rules win over plugin output for the same header name, and
//! the `Host` header always comes from the resolved endpoint.

use std::collections::BTreeMap;

use http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::model::{HeaderPassthrough, RequestHeaderRules, ResponseHeaderRules};

/// Headers that are consumed by the single transport hop and must not be
/// forwarded (RFC 9110 §7.6.1).
pub const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
    "host",
];

/// Headers owned by the gateway: they describe the *proxy* exchange and are
/// never forwarded upstream.
pub const GATEWAY_ROUTE_HEADERS: [&str; 3] = [
    "x-oagw-target-host",
    "x-oagw-error-source",
    "x-oagw-trace-id",
];

/// The headers forwarded even when `passthrough` is `none`: without them a
/// proxied body is unusable (no framing, no media type).
pub const CORE_PASSTHROUGH: [&str; 5] = [
    "accept",
    "accept-encoding",
    "content-type",
    "content-length",
    "user-agent",
];

/// Downstream credentials are never forwarded implicitly: the auth plugin
/// layer is the only thing that may put an `Authorization` header on an
/// outbound request.
pub const NEVER_PASSTHROUGH: [&str; 1] = ["authorization"];

/// The headers a protocol-upgrade handshake needs (RFC 6455 §4.1).
///
/// A handshake request *is* its protocol headers: a relay that drops
/// `Connection`/`Upgrade`/`Sec-WebSocket-*` does not forward a handshake at
/// all. They are carried onto every upgrade request, whatever the
/// `passthrough` posture is (PRD `cpt-cf-oagw-fr-streaming`).
pub const UPGRADE_HANDSHAKE_HEADERS: [&str; 6] = [
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// Copy the upgrade-handshake headers of `downstream` onto `outbound`.
///
/// Only headers that are missing (or appended to, for the multi-valued ones)
/// are touched: the passthrough posture may already have carried some of them.
pub fn carry_handshake_headers(outbound: &mut HeaderMap, downstream: &HeaderMap) {
    for name in UPGRADE_HANDSHAKE_HEADERS {
        let Ok(static_name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        for value in downstream.get_all(&static_name) {
            outbound.append(&static_name, value.clone());
        }
    }
}

/// True when `name` is a hop-by-hop header.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// True when `name` is a gateway-owned routing header.
#[must_use]
pub fn is_gateway_header(name: &str) -> bool {
    GATEWAY_ROUTE_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
}

/// The header names listed in a `Connection` header (comma-separated).
#[must_use]
pub fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(','))
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|token| !token.is_empty() && !token.eq_ignore_ascii_case("close"))
        .collect()
}

/// Strip every hop-by-hop header, the names the `Connection` header lists, and
/// the gateway's own routing headers.
///
/// `keep_upgrade` preserves `connection`/`upgrade` (WebSocket tunnelling).
pub fn strip_hop_by_hop(headers: &mut HeaderMap, keep_upgrade: bool) {
    let tokens = connection_tokens(headers);
    for token in &tokens {
        if !keep_upgrade
            || !(token.eq_ignore_ascii_case("upgrade") || token.eq_ignore_ascii_case("connection"))
        {
            headers.remove(token.as_str());
        }
    }
    for name in HOP_BY_HOP {
        if keep_upgrade && (name == "connection" || name == "upgrade") {
            continue;
        }
        headers.remove(name);
    }
    for name in GATEWAY_ROUTE_HEADERS {
        headers.remove(name);
    }
}

/// Parse a header name, rejecting invalid ones.
fn parse_name(name: &str) -> Option<HeaderName> {
    HeaderName::from_bytes(name.trim().as_bytes()).ok()
}

/// Parse a header value, rejecting anything that is not visible ASCII.
fn parse_value(value: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(value.trim()).ok()
}

/// Remove every header named by `names` (case-insensitive).
pub fn remove_all(headers: &mut HeaderMap, names: &[String]) {
    for name in names {
        headers.remove(name.trim());
    }
}

/// Apply a `set` map: overwrite (or create) each named header.
pub fn apply_set(headers: &mut HeaderMap, set: &BTreeMap<String, String>) {
    for (name, value) in set {
        let (Some(name), Some(value)) = (parse_name(name), parse_value(value)) else {
            continue;
        };
        headers.insert(name, value);
    }
}

/// Apply an `add` map: append to any existing value.
pub fn apply_add(headers: &mut HeaderMap, add: &BTreeMap<String, String>) {
    for (name, value) in add {
        let (Some(name), Some(value)) = (parse_name(name), parse_value(value)) else {
            continue;
        };
        headers.append(name, value);
    }
}

/// Project the downstream headers onto the outbound request according to
/// `passthrough` (DESIGN header-transformation table).
///
/// The result still contains hop-by-hop headers when they came from the
/// allowlist-independent core set only if they are body framing headers;
/// `strip_hop_by_hop` runs afterwards and has the final word.
#[must_use]
pub fn project_request_headers(downstream: &HeaderMap, rules: &RequestHeaderRules) -> HeaderMap {
    let mut outbound = HeaderMap::new();
    match rules.passthrough {
        HeaderPassthrough::All => {
            for (name, value) in downstream {
                outbound.append(name.clone(), value.clone());
            }
        }
        HeaderPassthrough::None | HeaderPassthrough::Allowlist => {
            for name in CORE_PASSTHROUGH {
                for value in downstream.get_all(name) {
                    outbound.append(HeaderName::from_static(name), value.clone());
                }
            }
            if rules.passthrough == HeaderPassthrough::Allowlist {
                for listed in &rules.passthrough_allowlist {
                    for value in downstream.get_all(listed.trim()) {
                        if let Some(name) = parse_name(listed) {
                            outbound.append(name, value.clone());
                        }
                    }
                }
            }
        }
    }
    for name in NEVER_PASSTHROUGH {
        let listed = rules
            .passthrough_allowlist
            .iter()
            .any(|l| l.trim().eq_ignore_ascii_case(name))
            || rules.set.contains_key(name)
            || rules.add.contains_key(name);
        if !listed {
            outbound.remove(name);
        }
    }
    remove_all(&mut outbound, &rules.remove);
    apply_set(&mut outbound, &rules.set);
    apply_add(&mut outbound, &rules.add);
    outbound
}

/// Apply the request-side rules to an already-built outbound header map.
///
/// Used after the auth/transform phases so configured rules take precedence.
pub fn apply_request_rules(headers: &mut HeaderMap, rules: &RequestHeaderRules) {
    remove_all(headers, &rules.remove);
    apply_set(headers, &rules.set);
    apply_add(headers, &rules.add);
}

/// Apply the response-side rules to an upstream response header map.
pub fn apply_response_rules(headers: &mut HeaderMap, rules: &ResponseHeaderRules) {
    remove_all(headers, &rules.remove);
    apply_set(headers, &rules.set);
    apply_add(headers, &rules.add);
}

/// Replace (or add) the `Host` header with the resolved endpoint's authority.
pub fn set_host(headers: &mut HeaderMap, authority: &str) {
    if let Ok(value) = HeaderValue::from_str(authority) {
        headers.insert(http::header::HOST, value);
    }
}

/// Every header value as a lossy UTF-8 string, joined for diagnostics.
#[must_use]
pub fn header_values(headers: &HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{HeaderRules, RequestHeaderRules};

    fn rules(json: &str) -> HeaderRules {
        serde_json::from_str(json).expect("valid header rules")
    }

    #[test]
    fn hop_by_hop_and_connection_tokens_are_stripped() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "x-secret, keep-alive".parse().unwrap());
        headers.insert("x-secret", "1".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("x-oagw-target-host", "internal".parse().unwrap());
        headers.insert("x-keep", "yes".parse().unwrap());
        strip_hop_by_hop(&mut headers, false);
        assert!(headers.get("x-secret").is_none());
        assert!(headers.get("connection").is_none());
        assert!(headers.get("transfer-encoding").is_none());
        assert!(headers.get("x-oagw-target-host").is_none());
        assert_eq!(headers.get("x-keep").unwrap(), "yes");
    }

    #[test]
    fn upgrade_headers_survive_when_tunneling() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", "Upgrade".parse().unwrap());
        headers.insert("upgrade", "websocket".parse().unwrap());
        strip_hop_by_hop(&mut headers, true);
        assert_eq!(headers.get("connection").unwrap(), "Upgrade");
        assert_eq!(headers.get("upgrade").unwrap(), "websocket");
    }

    #[test]
    fn default_passthrough_forwards_the_core_set_only() {
        let mut downstream = HeaderMap::new();
        downstream.insert("content-type", "application/json".parse().unwrap());
        downstream.insert("x-internal", "1".parse().unwrap());
        downstream.insert("authorization", "Bearer client-token".parse().unwrap());
        let projected = project_request_headers(&downstream, &RequestHeaderRules::default());
        assert_eq!(projected.get("content-type").unwrap(), "application/json");
        assert!(projected.get("x-internal").is_none());
        // Never forward the client's own credentials implicitly.
        assert!(projected.get("authorization").is_none());
    }

    #[test]
    fn allowlist_passthrough_is_exactly_the_allowlist() {
        let mut downstream = HeaderMap::new();
        downstream.insert("content-type", "application/json".parse().unwrap());
        downstream.insert("x-tenant", "acme".parse().unwrap());
        let rules = RequestHeaderRules {
            passthrough: HeaderPassthrough::Allowlist,
            passthrough_allowlist: vec!["X-Tenant".to_owned()],
            ..RequestHeaderRules::default()
        };
        let projected = project_request_headers(&downstream, &rules);
        assert_eq!(projected.get("x-tenant").unwrap(), "acme");
        // The core framing set is forwarded independently of the allowlist.
        assert_eq!(projected.get("content-type").unwrap(), "application/json");
        // Only the allowlist *plus* the core set is forwarded: anything else is
        // dropped.
        downstream.insert("x-internal", "1".parse().unwrap());
        assert!(
            project_request_headers(&downstream, &rules)
                .get("x-internal")
                .is_none()
        );
    }

    #[test]
    fn rules_apply_in_remove_set_add_order() {
        let mut downstream = HeaderMap::new();
        downstream.insert("x-drop", "1".parse().unwrap());
        downstream.insert("x-mode", "a".parse().unwrap());
        let configured = rules(
            r#"{"request": {"remove": ["x-drop"], "set": {"x-mode": "b"}, "add": {"x-extra": "v"}}}"#,
        );
        let mut outbound = project_request_headers(&downstream, &configured.request);
        apply_request_rules(&mut outbound, &configured.request);
        assert!(outbound.get("x-drop").is_none());
        assert_eq!(outbound.get("x-mode").unwrap(), "b");
        assert_eq!(outbound.get("x-extra").unwrap(), "v");
    }

    #[test]
    fn host_header_is_replaced() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "client.example".parse().unwrap());
        set_host(&mut headers, "api.internal:8443");
        assert_eq!(headers.get("host").unwrap(), "api.internal:8443");
    }

    #[test]
    fn invalid_rule_values_are_ignored() {
        let mut headers = HeaderMap::new();
        let mut set = BTreeMap::new();
        set.insert("bad name\n".to_owned(), "x".to_owned());
        apply_set(&mut headers, &set);
        assert!(headers.is_empty());
    }
}
