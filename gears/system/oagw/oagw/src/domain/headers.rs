// Created: 2026-09-01 by Constructor Tech
//! Header transformation.
//!
//! `docs/DESIGN.md` §3.2 "Headers Transformation": routing headers are
//! consumed by the gateway, hop-by-hop headers are always stripped, and
//! what else survives is decided by the upstream `headers` configuration.

use http::HeaderMap;

use super::model::{HeaderRules, HeadersConfig, Passthrough};

/// Inbound headers consumed for routing and never forwarded upstream.
pub const ROUTING_HEADERS: [&str; 1] = ["x-oagw-target-host"];

/// Headers removed from every proxied request, per the HTTP spec.
pub const HOP_BY_HOP_HEADERS: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "proxy-connection",
];

/// Build the upstream request headers from the inbound set.
///
/// Routing and hop-by-hop headers go first; what remains depends on
/// `passthrough`; then `set`/`add`/`remove` are applied.
#[must_use]
pub fn transform_request(inbound: &HeaderMap, config: Option<&HeadersConfig>) -> HeaderMap {
    let rules = config.and_then(|c| c.request.as_ref());
    let mut out = HeaderMap::new();

    match rules.map(|r| r.passthrough).unwrap_or_default() {
        Passthrough::None => {}
        Passthrough::Allowlist => {
            let allowlist = rules
                .map(|r| r.passthrough_allowlist.as_slice())
                .unwrap_or(&[]);
            for name in allowlist {
                for value in inbound.get_all(name) {
                    if let Ok(name) = http::header::HeaderName::from_bytes(name.as_bytes()) {
                        out.append(name, value.clone());
                    }
                }
            }
        }
        Passthrough::All => {
            for (name, value) in inbound {
                if is_stripped(name.as_str()) {
                    continue;
                }
                out.append(name.clone(), value.clone());
            }
        }
    }

    if let Some(rules) = rules {
        apply_rules(&mut out, inbound, rules);
    }

    // `Host` is always replaced by the upstream authority, never inherited
    // from the inbound request.
    out.remove(http::header::HOST);
    out.remove(http::header::CONTENT_LENGTH);
    // The body is forwarded even when no header is, and its media type is
    // part of the body: a JSON payload that reaches the upstream without
    // `Content-Type` is not the request the caller sent (DESIGN.md §3.2
    // "Transformation Rules" puts `Content-Type` among the well-known
    // headers the gateway sets rather than drops). An explicit `remove`
    // still wins.
    let removed = rules.is_some_and(|rules| {
        rules
            .remove
            .iter()
            .any(|name| name.eq_ignore_ascii_case("content-type"))
    });
    if !removed && let Some(value) = inbound.get(http::header::CONTENT_TYPE) {
        out.entry(http::header::CONTENT_TYPE)
            .or_insert(value.clone());
    }
    out
}

/// Build the client response headers from the upstream response.
#[must_use]
pub fn transform_response(upstream: &HeaderMap, config: Option<&HeadersConfig>) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream {
        let name = name.as_str();
        if is_stripped(name) || is_entity_header(name) {
            continue;
        }
        if let Ok(name) = http::header::HeaderName::from_bytes(name.as_bytes()) {
            out.append(name, value.clone());
        }
    }
    if let Some(rules) = config.and_then(|c| c.response.as_ref()) {
        for name in &rules.remove {
            out.remove(name);
        }
        for (name, value) in &rules.set {
            if let (Ok(name), Ok(value)) = (
                http::header::HeaderName::from_bytes(name.as_bytes()),
                http::header::HeaderValue::from_str(value),
            ) {
                out.insert(name, value);
            }
        }
        for (name, value) in &rules.add {
            if let (Ok(name), Ok(value)) = (
                http::header::HeaderName::from_bytes(name.as_bytes()),
                http::header::HeaderValue::from_str(value),
            ) {
                out.append(name, value);
            }
        }
    }
    out
}

/// `true` when the gateway consumes the header rather than forwarding it.
#[must_use]
pub fn is_stripped(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ROUTING_HEADERS.contains(&lower.as_str()) || HOP_BY_HOP_HEADERS.contains(&lower.as_str())
}

/// `true` for headers the HTTP library recomputes itself.
fn is_entity_header(name: &str) -> bool {
    matches!(
        name,
        "content-length" | "transfer-encoding" | "connection" | "keep-alive"
    )
}

fn apply_rules(out: &mut HeaderMap, inbound: &HeaderMap, rules: &HeaderRules) {
    for name in &rules.remove {
        out.remove(name);
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_str(value),
        ) {
            out.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_str(value),
        ) {
            out.append(name, value);
        }
    }
    let _ = inbound;
}

/// Headers an upgrade is negotiated with, preserved even though `Upgrade`
/// and `Connection` are hop-by-hop by name.
const UPGRADE_HEADERS: [&str; 6] = [
    "upgrade",
    "connection",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// Build the upstream request headers for a protocol upgrade.
///
/// The same as [`transform_request`] except that the handshake headers are
/// re-added afterwards: `Upgrade` and `Connection` name exactly the headers
/// the ordinary transformation strips, and without the client's
/// `Sec-WebSocket-Key` the upstream cannot answer the handshake at all.
#[must_use]
pub fn transform_upgrade_request(inbound: &HeaderMap, config: Option<&HeadersConfig>) -> HeaderMap {
    let mut out = transform_request(inbound, config);
    for name in UPGRADE_HEADERS {
        for value in inbound.get_all(name) {
            if let Ok(name) = http::header::HeaderName::from_bytes(name.as_bytes()) {
                out.append(name, value.clone());
            }
        }
    }
    out
}

/// A header map as an ordered list of pairs, the shape plugins see.
#[must_use]
pub fn flatten(map: &HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// The `x-request-id` value carried by `inbound`, or a freshly minted one.
#[must_use]
pub fn resolve_request_id(inbound: &HeaderMap, generated: &str) -> String {
    inbound
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| generated.to_owned())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{HeaderRules, HeadersConfig, Passthrough};
    use std::collections::BTreeMap;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (n, v) in pairs {
            m.insert(
                http::header::HeaderName::from_bytes(n.as_bytes()).expect("name"),
                http::header::HeaderValue::from_str(v).expect("value"),
            );
        }
        m
    }

    #[test]
    fn an_upgrade_keeps_its_handshake_headers() {
        let inbound = map(&[
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("x-oagw-target-host", "us.vendor.com"),
        ]);
        let out = transform_upgrade_request(&inbound, None);
        assert_eq!(
            out.get("upgrade").and_then(|v| v.to_str().ok()),
            Some("websocket")
        );
        assert_eq!(
            out.get("sec-websocket-key").and_then(|v| v.to_str().ok()),
            Some("dGhlIHNhbXBsZSBub25jZQ==")
        );
        assert_eq!(
            out.get("sec-websocket-version")
                .and_then(|v| v.to_str().ok()),
            Some("13")
        );
        // The routing header is still gone.
        assert!(out.get("x-oagw-target-host").is_none());
    }

    #[test]
    fn the_default_forwards_nothing() {
        let inbound = map(&[("authorization", "Bearer x"), ("x-custom", "1")]);
        let out = transform_request(&inbound, None);
        assert!(out.is_empty());
    }

    #[test]
    fn the_body_media_type_survives_a_headerless_default() {
        // The body is forwarded even when no header is, and a JSON payload
        // without its `Content-Type` is not the request the caller sent.
        let inbound = map(&[("content-type", "application/json"), ("x-custom", "1")]);
        let out = transform_request(&inbound, None);
        assert!(out.get("x-custom").is_none());
        assert_eq!(
            out.get("content-type"),
            Some(&"application/json".parse().expect("v"))
        );
    }

    #[test]
    fn an_explicit_remove_beats_the_media_type_preservation() {
        let inbound = map(&[("content-type", "application/json")]);
        let rules = HeaderRules {
            remove: vec!["content-type".to_owned()],
            passthrough: Passthrough::All,
            ..HeaderRules::default()
        };
        let config = HeadersConfig {
            request: Some(rules),
            response: None,
        };
        let out = transform_request(&inbound, Some(&config));
        assert!(out.get("content-type").is_none());
    }

    #[test]
    fn a_rule_can_override_the_media_type() {
        let inbound = map(&[("content-type", "text/plain")]);
        let mut set = BTreeMap::new();
        set.insert("content-type".to_owned(), "application/json".to_owned());
        let rules = HeaderRules {
            set,
            ..HeaderRules::default()
        };
        let config = HeadersConfig {
            request: Some(rules),
            response: None,
        };
        let out = transform_request(&inbound, Some(&config));
        assert_eq!(
            out.get("content-type"),
            Some(&"application/json".parse().expect("v"))
        );
    }

    #[test]
    fn content_length_is_always_recomputed() {
        let inbound = map(&[
            ("content-length", "7"),
            ("content-type", "application/json"),
        ]);
        let out = transform_request(&inbound, None);
        assert!(out.get("content-length").is_none());
        assert!(out.get("content-type").is_some());
    }

    #[test]
    fn routing_and_hop_by_hop_headers_never_reach_the_upstream() {
        let inbound = map(&[
            ("x-oagw-target-host", "us.vendor.com"),
            ("host", "gateway.internal"),
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("x-keep", "yes"),
        ]);
        let rules = HeaderRules {
            passthrough: Passthrough::All,
            ..HeaderRules::default()
        };
        let config = HeadersConfig {
            request: Some(rules),
            response: None,
        };
        let out = transform_request(&inbound, Some(&config));
        assert!(out.get("x-oagw-target-host").is_none());
        assert!(out.get("host").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("transfer-encoding").is_none());
        assert!(out.get("upgrade").is_none());
        assert_eq!(out.get("x-keep"), Some(&"yes".parse().expect("v")));
    }

    #[test]
    fn allowlist_forwards_only_listed_headers() {
        let inbound = map(&[
            ("authorization", "Bearer x"),
            ("x-trace", "abc"),
            ("x-other", "no"),
        ]);
        let rules = HeaderRules {
            passthrough: Passthrough::Allowlist,
            passthrough_allowlist: vec!["authorization".to_owned()],
            ..HeaderRules::default()
        };
        let config = HeadersConfig {
            request: Some(rules),
            response: None,
        };
        let out = transform_request(&inbound, Some(&config));
        assert_eq!(
            out.get("authorization"),
            Some(&"Bearer x".parse().expect("v"))
        );
        assert!(out.get("x-trace").is_none());
        assert!(out.get("x-other").is_none());
    }

    #[test]
    fn set_add_and_remove_apply_in_order() {
        let inbound = map(&[("x-a", "old"), ("x-b", "keep")]);
        let mut set = BTreeMap::new();
        set.insert("x-a".to_owned(), "new".to_owned());
        set.insert("x-c".to_owned(), "created".to_owned());
        let mut add = BTreeMap::new();
        add.insert("x-b".to_owned(), "extra".to_owned());
        let rules = HeaderRules {
            set,
            add,
            remove: vec!["x-b".to_owned()],
            passthrough: Passthrough::All,
            passthrough_allowlist: Vec::new(),
        };
        let config = HeadersConfig {
            request: Some(rules),
            response: None,
        };
        let out = transform_request(&inbound, Some(&config));
        assert_eq!(out.get("x-a"), Some(&"new".parse().expect("v")));
        assert_eq!(out.get("x-c"), Some(&"created".parse().expect("v")));
        // `remove` clears the passthrough copy, then `add` re-adds one.
        let values: Vec<_> = out.get_all("x-b").iter().collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], "extra");
    }

    #[test]
    fn response_transformations_apply_and_entity_headers_are_dropped() {
        let upstream = map(&[
            ("content-type", "application/json"),
            ("content-length", "12"),
            ("x-set", "old"),
            ("x-drop", "gone"),
        ]);
        let mut set = BTreeMap::new();
        set.insert("x-set".to_owned(), "new".to_owned());
        let config = HeadersConfig {
            request: None,
            response: Some(HeaderRules {
                set,
                remove: vec!["x-drop".to_owned()],
                ..HeaderRules::default()
            }),
        };
        let out = transform_response(&upstream, Some(&config));
        assert_eq!(
            out.get("content-type"),
            Some(&"application/json".parse().expect("v"))
        );
        assert!(out.get("content-length").is_none());
        assert_eq!(out.get("x-set"), Some(&"new".parse().expect("v")));
        assert!(out.get("x-drop").is_none());
    }

    #[test]
    fn request_ids_propagate_or_are_generated() {
        let present = map(&[("x-request-id", "abc")]);
        assert_eq!(resolve_request_id(&present, "gen"), "abc");
        let absent = map(&[]);
        assert_eq!(resolve_request_id(&absent, "gen"), "gen");
    }
}
