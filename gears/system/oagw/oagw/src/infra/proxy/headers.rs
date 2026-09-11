//! Header transformation for the proxy data plane.
//!
//! Implements `docs/PRD.md` FR "Header transformation" and the DESIGN
//! hop-by-hop stripping list.

use http::HeaderMap;

/// Hop-by-hop headers that are never forwarded, per the PRD.
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Header naming the upstream endpoint to target.
pub const TARGET_HOST: &str = "x-oagw-target-host";
/// Header distinguishing gateway errors from upstream responses.
pub const ERROR_SOURCE: &str = "x-oagw-error-source";
/// Request-id header propagated by the built-in transform plugin.
pub const REQUEST_ID: &str = "x-request-id";
/// Header carrying the browser's origin, part of the WebSocket handshake.
pub const ORIGIN: &str = "origin";
/// Bucket capacity reported on a rate-limited upstream.
pub const RATE_LIMIT: &str = "x-ratelimit-limit";
/// Tokens left in the bucket after this request.
pub const RATE_REMAINING: &str = "x-ratelimit-remaining";
/// Seconds until the bucket is replenished.
pub const RATE_RESET: &str = "x-ratelimit-reset";
/// Inbound headers the proxy always needs to describe the body it forwards.
const BODY_HEADERS: &[&str] = &["content-type", "content-length"];
/// Prefix of the headers a WebSocket handshake cannot do without.
const WEBSOCKET_PREFIX: &str = "sec-websocket";

/// Whether a header name is in the hop-by-hop set.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| h.eq_ignore_ascii_case(name))
}

/// Whether an inbound header belongs to the WebSocket handshake.
///
/// A tunnelled upgrade cannot be completed without its own headers, so they
/// survive every passthrough mode.
#[must_use]
pub fn is_handshake_header(name: &str) -> bool {
    name.starts_with(WEBSOCKET_PREFIX) || name.eq_ignore_ascii_case(ORIGIN)
}

/// Remove every hop-by-hop header and the OAGW routing header.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    headers.remove(TARGET_HOST);
}

/// Build the outbound request headers from the inbound request.
///
/// The inbound set is the starting point; `passthrough` then decides how much
/// of it survives, before `set`/`add`/`remove` are applied.
#[must_use]
pub fn build_outbound_request(
    inbound: &HeaderMap,
    rules: &crate::domain::model::HeaderRequestRules,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        let name_str = name.as_str();
        if is_hop_by_hop(name_str) || name_str.eq_ignore_ascii_case(TARGET_HOST) {
            continue;
        }
        let keep = match rules.passthrough {
            crate::domain::model::PassthroughMode::All => true,
            crate::domain::model::PassthroughMode::Allowlist => rules
                .passthrough_allowlist
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name_str)),
            crate::domain::model::PassthroughMode::None => {
                BODY_HEADERS
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(name_str))
                    || is_handshake_header(name_str)
            }
        };
        if keep {
            out.append(name.clone(), value.clone());
        }
    }
    for name in &rules.remove {
        out.remove(name);
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::from_str(value),
        ) {
            out.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::from_str(value),
        ) {
            out.append(name, value);
        }
    }
    out
}

/// Apply the response rules to the upstream response headers.
#[must_use]
pub fn build_outbound_response(
    inbound: &HeaderMap,
    rules: &crate::domain::model::HeaderResponseRules,
) -> HeaderMap {
    let mut out = inbound.clone();
    for name in &rules.remove {
        out.remove(name);
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::from_str(value),
        ) {
            out.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::try_from(name.as_str()),
            http::HeaderValue::from_str(value),
        ) {
            out.append(name, value);
        }
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::model::{HeaderRequestRules, HeaderResponseRules, PassthroughMode};
    use std::collections::BTreeMap;

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let name = http::HeaderName::from_bytes(name.as_bytes()).unwrap();
            let value = http::HeaderValue::from_str(value).unwrap();
            map.append(name, value);
        }
        map
    }

    fn rules() -> HeaderRequestRules {
        HeaderRequestRules::default()
    }

    #[test]
    fn default_passthrough_keeps_only_body_headers() {
        let inbound = header_map(&[
            ("content-type", "application/json"),
            ("x-secret", "1"),
            ("authorization", "Bearer x"),
        ]);
        let out = build_outbound_request(&inbound, &rules());
        assert!(out.contains_key("content-type"));
        assert!(!out.contains_key("x-secret"));
        assert!(!out.contains_key("authorization"));
    }

    #[test]
    fn allowlist_forwards_only_listed_names() {
        let inbound = header_map(&[("x-keep", "1"), ("x-drop", "1")]);
        let mut r = rules();
        r.passthrough = PassthroughMode::Allowlist;
        r.passthrough_allowlist = vec!["x-keep".to_owned()];
        let out = build_outbound_request(&inbound, &r);
        assert!(out.contains_key("x-keep"));
        assert!(!out.contains_key("x-drop"));
    }

    #[test]
    fn all_forwards_everything_but_hop_by_hop() {
        let inbound = header_map(&[
            ("x-keep", "1"),
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("x-oagw-target-host", "us.vendor.com"),
        ]);
        let mut r = rules();
        r.passthrough = PassthroughMode::All;
        let out = build_outbound_request(&inbound, &r);
        assert!(out.contains_key("x-keep"));
        assert!(!out.contains_key("connection"));
        assert!(!out.contains_key("transfer-encoding"));
        assert!(!out.contains_key("x-oagw-target-host"));
    }

    #[test]
    fn set_overrides_and_add_appends() {
        let inbound = header_map(&[("x-a", "old"), ("x-b", "b")]);
        let mut set: BTreeMap<String, String> = BTreeMap::new();
        set.insert("x-a".to_owned(), "new".to_owned());
        let mut add: BTreeMap<String, String> = BTreeMap::new();
        add.insert("x-b".to_owned(), "extra".to_owned());
        let mut r = rules();
        r.passthrough = crate::domain::model::PassthroughMode::All;
        r.set = set;
        r.add = add;
        let out = build_outbound_request(&inbound, &r);
        assert_eq!(out.get("x-a").unwrap(), "new");
        assert_eq!(out.get_all("x-b").iter().count(), 2);
    }

    #[test]
    fn remove_drops_inbound_headers() {
        let inbound = header_map(&[("x-a", "1"), ("x-b", "2")]);
        let mut r = rules();
        r.remove = vec!["x-a".to_owned()];
        r.passthrough = PassthroughMode::All;
        let out = build_outbound_request(&inbound, &r);
        assert!(!out.contains_key("x-a"));
        assert!(out.contains_key("x-b"));
    }

    #[test]
    fn response_rules_are_applied() {
        let inbound = header_map(&[("x-internal", "secret"), ("x-keep", "1")]);
        let r = HeaderResponseRules {
            remove: vec!["x-internal".to_owned()],
            ..HeaderResponseRules::default()
        };
        let out = build_outbound_response(&inbound, &r);
        assert!(!out.contains_key("x-internal"));
        assert!(out.contains_key("x-keep"));
    }

    #[test]
    fn hop_by_hop_detection_is_case_insensitive() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("TRANSFER-ENCODING"));
        assert!(!is_hop_by_hop("content-type"));
    }
}
