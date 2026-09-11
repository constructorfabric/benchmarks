//! Header transformation on the proxy path.
//!
//! Realizes `cpt-cf-oagw-algo-ph-header-transform`.
//!
//! Three categories are handled distinctly: routing headers are consumed by the
//! gateway and never forwarded; hop-by-hop headers are stripped in both
//! directions; everything else is forwarded according to the upstream's
//! configured passthrough mode.

use hyper::HeaderMap;
use hyper::header::{HeaderName, HeaderValue};

use crate::domain::error::TARGET_HOST_HEADER;
use crate::domain::model::{HeaderRules, Passthrough};

/// Connection-scoped headers, stripped in both directions.
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

/// Headers the gateway consumes for its own routing decisions.
pub const ROUTING: [&str; 1] = [TARGET_HOST_HEADER];

/// Whether a header is hop-by-hop.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
}

/// Whether a header is consumed by the gateway rather than forwarded.
#[must_use]
pub fn is_routing(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ROUTING.contains(&lower.as_str())
}

/// Build the header map to send upstream.
///
/// `preserve_upgrade` keeps `Connection` and `Upgrade` in place, which is the
/// one documented exception to hop-by-hop stripping: those two headers are
/// exactly what makes a WebSocket upgrade work.
// @cpt-begin:cpt-cf-oagw-dod-ph-header-transform:p1:inst-full
#[must_use]
pub fn build_request_headers(
    inbound: &HeaderMap,
    rules: &HeaderRules,
    authority: &str,
    preserve_upgrade: bool,
) -> HeaderMap {
    let mut out = HeaderMap::new();

    for (name, value) in inbound {
        let n = name.as_str().to_ascii_lowercase();
        if is_routing(&n) {
            continue;
        }
        if is_hop_by_hop(&n) && !(preserve_upgrade && matches!(n.as_str(), "connection" | "upgrade"))
        {
            continue;
        }
        if n == "host" {
            continue; // replaced with the upstream authority below
        }
        if rules.remove.iter().any(|r| r.eq_ignore_ascii_case(&n)) {
            continue;
        }
        let forward = match rules.passthrough {
            Passthrough::All => true,
            Passthrough::None => {
                // Even with nothing passed through, the headers that make the
                // exchange itself work must survive.
                is_essential(&n) || (preserve_upgrade && is_upgrade_related(&n))
            }
            Passthrough::Allowlist => {
                rules
                    .passthrough_allowlist
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&n))
                    || is_essential(&n)
                    || (preserve_upgrade && is_upgrade_related(&n))
            }
        };
        if forward {
            out.append(name.clone(), value.clone());
        }
    }

    apply_set_and_add(&mut out, rules);

    if let Ok(v) = HeaderValue::from_str(authority) {
        out.insert(hyper::header::HOST, v);
    }
    out
}
// @cpt-end:cpt-cf-oagw-dod-ph-header-transform:p1:inst-full

/// Headers without which the exchange cannot be framed or understood.
///
/// A documented, deliberate exception to `passthrough: none`: a forwarded body
/// is uninterpretable without its own framing headers, so these four survive
/// every passthrough mode. This mirrors the `Upgrade`/`Connection` exception on
/// the upgrade path and is recorded in `proxy-http.md`.
fn is_essential(name: &str) -> bool {
    matches!(
        name,
        "content-type" | "content-length" | "accept" | "accept-encoding"
    )
}

/// Headers that carry the WebSocket handshake.
fn is_upgrade_related(name: &str) -> bool {
    name.starts_with("sec-websocket") || matches!(name, "connection" | "upgrade")
}

/// Build the header map to relay back to the client.
#[must_use]
pub fn build_response_headers(
    upstream: &HeaderMap,
    rules: &HeaderRules,
    preserve_upgrade: bool,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream {
        let n = name.as_str().to_ascii_lowercase();
        if is_hop_by_hop(&n) && !(preserve_upgrade && matches!(n.as_str(), "connection" | "upgrade"))
        {
            continue;
        }
        if rules.remove.iter().any(|r| r.eq_ignore_ascii_case(&n)) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    apply_set_and_add(&mut out, rules);
    out
}

fn apply_set_and_add(out: &mut HeaderMap, rules: &HeaderRules) {
    for (k, v) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            out.insert(name, value);
        }
    }
    for (k, v) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            out.append(name, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn inbound(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn rules(mode: Passthrough) -> HeaderRules {
        HeaderRules {
            set: BTreeMap::new(),
            add: BTreeMap::new(),
            remove: vec![],
            passthrough: mode,
            passthrough_allowlist: vec![],
        }
    }

    #[test]
    fn every_hop_by_hop_header_is_stripped() {
        let pairs: Vec<(&str, &str)> = HOP_BY_HOP.iter().map(|h| (*h, "x")).collect();
        let out = build_request_headers(&inbound(&pairs), &rules(Passthrough::All), "u:80", false);
        for h in HOP_BY_HOP {
            assert!(out.get(h).is_none(), "{h} should have been stripped");
        }
    }

    #[test]
    fn the_routing_header_is_consumed_not_forwarded() {
        let out = build_request_headers(
            &inbound(&[(TARGET_HOST_HEADER, "a.example"), ("x-keep", "1")]),
            &rules(Passthrough::All),
            "u:80",
            false,
        );
        assert!(out.get(TARGET_HOST_HEADER).is_none());
        assert_eq!(out.get("x-keep").unwrap(), "1");
    }

    #[test]
    fn host_is_replaced_with_the_upstream_authority() {
        let out = build_request_headers(
            &inbound(&[("host", "gateway.example")]),
            &rules(Passthrough::All),
            "upstream.example:8443",
            false,
        );
        assert_eq!(out.get("host").unwrap(), "upstream.example:8443");
    }

    #[test]
    fn passthrough_none_drops_ordinary_headers_but_keeps_framing() {
        let out = build_request_headers(
            &inbound(&[("x-custom", "1"), ("content-type", "application/json")]),
            &rules(Passthrough::None),
            "u:80",
            false,
        );
        assert!(out.get("x-custom").is_none());
        assert_eq!(out.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn passthrough_allowlist_forwards_only_named_headers() {
        let mut r = rules(Passthrough::Allowlist);
        r.passthrough_allowlist = vec!["x-wanted".to_owned()];
        let out = build_request_headers(
            &inbound(&[("x-wanted", "1"), ("x-unwanted", "2")]),
            &r,
            "u:80",
            false,
        );
        assert_eq!(out.get("x-wanted").unwrap(), "1");
        assert!(out.get("x-unwanted").is_none());
    }

    #[test]
    fn remove_wins_over_passthrough() {
        let mut r = rules(Passthrough::All);
        r.remove = vec!["X-Secret".to_owned()];
        let out = build_request_headers(&inbound(&[("x-secret", "s")]), &r, "u:80", false);
        assert!(out.get("x-secret").is_none());
    }

    #[test]
    fn set_overwrites_and_add_appends() {
        let mut r = rules(Passthrough::All);
        r.set.insert("x-a".to_owned(), "set".to_owned());
        r.add.insert("x-b".to_owned(), "added".to_owned());
        let out = build_request_headers(&inbound(&[("x-a", "original")]), &r, "u:80", false);
        assert_eq!(out.get("x-a").unwrap(), "set");
        assert_eq!(out.get("x-b").unwrap(), "added");
    }

    #[test]
    fn an_upgrade_exchange_keeps_connection_and_upgrade() {
        let out = build_request_headers(
            &inbound(&[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "abc"),
                ("sec-websocket-version", "13"),
            ]),
            &rules(Passthrough::None),
            "u:80",
            true,
        );
        assert_eq!(out.get("upgrade").unwrap(), "websocket");
        assert_eq!(out.get("connection").unwrap(), "Upgrade");
        assert_eq!(out.get("sec-websocket-key").unwrap(), "abc");
        assert_eq!(out.get("sec-websocket-version").unwrap(), "13");
    }

    #[test]
    fn a_non_upgrade_exchange_still_strips_them() {
        let out = build_request_headers(
            &inbound(&[("connection", "keep-alive"), ("upgrade", "h2c")]),
            &rules(Passthrough::All),
            "u:80",
            false,
        );
        assert!(out.get("upgrade").is_none());
        assert!(out.get("connection").is_none());
    }

    #[test]
    fn response_headers_strip_hop_by_hop_and_apply_rules() {
        let mut r = rules(Passthrough::All);
        r.set.insert("x-added".to_owned(), "1".to_owned());
        let out = build_response_headers(
            &inbound(&[("transfer-encoding", "chunked"), ("content-type", "text/plain")]),
            &r,
            false,
        );
        assert!(out.get("transfer-encoding").is_none());
        assert_eq!(out.get("content-type").unwrap(), "text/plain");
        assert_eq!(out.get("x-added").unwrap(), "1");
    }
}
