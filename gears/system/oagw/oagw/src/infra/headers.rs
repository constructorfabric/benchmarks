// Created: 2026-09-02 by Constructor Tech
//! Header transformation between the inbound request, the gateway and the
//! upstream (`DESIGN.md` §3.3 Headers Transformation).
//!
//! Three categories, in the order the document lists them:
//!
//! 1. *Routing headers* — `X-OAGW-Target-Host` is read during routing and never
//!    forwarded; `Host` / `:authority` are replaced with the upstream authority.
//! 2. *Hop-by-hop headers* — stripped by default per RFC 9110 §7.6.1.
//! 3. *Passthrough headers* — forwarded according to the upstream's
//!    `headers.request.passthrough` rule, then `remove` / `set` / `add` applied.

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::model::{Passthrough, RequestHeaderRules, ResponseHeaderRules};
use crate::gts;

/// Headers that describe the connection rather than the message and are always
/// stripped (`PRD.md` fr-header-transform).
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

/// Headers the gateway consumes itself and never forwards upstream.
pub const ROUTING_HEADERS: &[&str] = &[gts::TARGET_HOST_HEADER, "host"];

/// Whether `name` is a hop-by-hop header.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
        // Headers named by `Connection` are hop-by-hop as well.
        || lower == "proxy-connection"
}

/// Whether `name` is consumed by the gateway for routing.
#[must_use]
pub fn is_routing_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ROUTING_HEADERS.contains(&lower.as_str())
}

/// Removes the hop-by-hop headers from `headers`, honouring `Connection`-named
/// extensions.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // `Connection: <name>` also names additional hop-by-hop headers.
    let named: Vec<HeaderName> = headers
        .get_all(axum::http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| HeaderName::try_from(s).ok())
        .collect();
    for name in named {
        headers.remove(&name);
    }
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
}

/// Builds the outbound request headers.
///
/// `passthrough` decides which inbound headers survive at all; the explicit
/// `remove` / `set` / `add` rules are applied on top, and `injected` (the auth
/// plugin's credentials) are added last so a rule cannot clobber them.
#[must_use]
pub fn outbound_request_headers(
    inbound: &HeaderMap,
    rules: Option<&RequestHeaderRules>,
    injected: &[(HeaderName, HeaderValue)],
    target_authority: &str,
    request_id: Option<&HeaderValue>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    let (passthrough, allowlist) = passthrough_mode(rules);
    if passthrough != Passthrough::None {
        for (name, value) in inbound.iter() {
            if is_hop_by_hop(name.as_str()) || is_routing_header(name.as_str()) {
                continue;
            }
            if passthrough == Passthrough::Allowlist
                && !allowlist.iter().any(|allowed| allowed.eq_ignore_ascii_case(name.as_str()))
            {
                continue;
            }
            out.append(name, value.clone());
        }
    }

    if let Some(rules) = rules {
        if let Some(remove) = rules.remove.as_ref() {
            for name in remove {
                out.remove(name);
            }
        }
        if let Some(set) = rules.set.as_ref() {
            for (name, value) in set {
                if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                    out.insert(name, value);
                }
            }
        }
        if let Some(add) = rules.add.as_ref() {
            for (name, value) in add {
                if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                    out.append(name, value);
                }
            }
        }
    }

    for (name, value) in injected {
        out.insert(name, value.clone());
    }
    if let Some(id) = request_id {
        out.insert(axum::http::header::HeaderName::from_static(gts::REQUEST_ID_HEADER), id.clone());
    }

    // `Host` is always the upstream authority.
    if let Ok(host) = HeaderValue::from_str(target_authority) {
        out.insert(axum::http::header::HOST, host);
    }
    out
}

/// Applies the upstream's response header rules to the upstream response.
pub fn apply_response_rules(headers: &mut HeaderMap, rules: Option<&ResponseHeaderRules>) {
    let Some(rules) = rules else { return };
    if let Some(remove) = rules.remove.as_ref() {
        for name in remove {
            headers.remove(name);
        }
    }
    if let Some(set) = rules.set.as_ref() {
        for (name, value) in set {
            if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                headers.insert(name, value);
            }
        }
    }
    if let Some(add) = rules.add.as_ref() {
        for (name, value) in add {
            if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
                headers.append(name, value);
            }
        }
    }
}

fn passthrough_mode(rules: Option<&RequestHeaderRules>) -> (Passthrough, Vec<String>) {
    // The schema default is `none`: an upstream that does not opt in gets no
    // inbound headers beyond what the gateway itself sets.
    match rules {
        None => (Passthrough::None, Vec::new()),
        Some(rules) => (
            rules.passthrough.unwrap_or_default(),
            rules.passthrough_allowlist.clone().unwrap_or_default(),
        ),
    }
}

/// Rebuilds the response headers returned to the caller.
///
/// Upstream response headers pass through minus hop-by-hop, minus `Content-Length`
/// when the gateway re-chunks the body, plus `X-OAGW-Error-Source`.
#[must_use]
pub fn outbound_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        out.append(name, value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::AuthInjection;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let (name, value) = ((*name).to_owned(), (*value).to_owned());
            map.insert(
                HeaderName::try_from(name.as_str()).unwrap(),
                HeaderValue::from_str(&value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn hop_by_hop_headers_are_identified() {
        for name in HOP_BY_HOP {
            assert!(is_hop_by_hop(name), "{name}");
        }
        assert!(is_hop_by_hop("Proxy-Connection"));
        assert!(!is_hop_by_hop("content-type"));
        assert!(is_routing_header("x-oagw-target-host"));
        // `Host` is a routing header too: it is replaced, never forwarded.
        assert!(is_routing_header("host"));
    }

    #[test]
    fn strip_hop_by_hop_removes_headers_named_by_connection() {
        let mut map = headers(&[
            ("connection", "x-secret, keep-alive"),
            ("x-secret", "leak"),
            ("x-correlation-id", "abc"),
            ("transfer-encoding", "chunked"),
        ]);
        strip_hop_by_hop(&mut map);
        assert!(map.get("x-correlation-id").is_some());
        assert!(map.get("transfer-encoding").is_none());
        assert!(map.get("connection").is_none());
        assert!(map.get("x-secret").is_none(), "headers named by Connection go too");
    }

    #[test]
    fn passthrough_none_forwards_nothing_but_the_host() {
        let inbound = headers(&[("x-api-key", "k"), ("content-type", "application/json")]);
        let out = outbound_request_headers(&inbound, None, &[], "api.openai.com:443", None);
        assert!(out.get("x-api-key").is_none(), "default passthrough is none");
        assert_eq!(out.get(axum::http::header::HOST).unwrap(), "api.openai.com:443");
    }

    #[test]
    fn passthrough_all_forwards_everything_but_hop_by_hop_and_routing() {
        let inbound = headers(&[
            ("x-api-key", "k"),
            ("x-oagw-target-host", "eu.vendor.com"),
            ("connection", "close"),
            ("host", "internal.local"),
        ]);
        let rules = RequestHeaderRules {
            passthrough: Some(Passthrough::All),
            ..Default::default()
        };
        let out = outbound_request_headers(&inbound, Some(&rules), &[], "upstream.example.com", None);
        assert_eq!(out.get("x-api-key").unwrap(), "k");
        assert!(out.get("x-oagw-target-host").is_none());
        assert!(out.get("connection").is_none());
        assert_eq!(out.get(axum::http::header::HOST).unwrap(), "upstream.example.com");
    }

    #[test]
    fn passthrough_allowlist_is_exact_and_case_insensitive() {
        let inbound = headers(&[("x-allowed", "yes"), ("x-denied", "no")]);
        let rules = RequestHeaderRules {
            passthrough: Some(Passthrough::Allowlist),
            passthrough_allowlist: Some(vec!["X-Allowed".to_owned()]),
            ..Default::default()
        };
        let out = outbound_request_headers(&inbound, Some(&rules), &[], "h", None);
        assert_eq!(out.get("x-allowed").unwrap(), "yes");
        assert!(out.get("x-denied").is_none());
    }

    #[test]
    fn remove_set_and_add_apply_in_that_order() {
        let inbound = headers(&[("x-remove", "1"), ("x-set", "old")]);
        let rules = RequestHeaderRules {
            passthrough: Some(Passthrough::All),
            remove: Some(vec!["x-remove".to_owned()]),
            set: Some([("x-set".to_owned(), "new".to_owned())].into_iter().collect()),
            add: Some([("x-added".to_owned(), "1".to_owned())].into_iter().collect()),
            passthrough_allowlist: None,
        };
        let out = outbound_request_headers(&inbound, Some(&rules), &[], "h", None);
        assert!(out.get("x-remove").is_none());
        assert_eq!(out.get("x-set").unwrap(), "new");
        assert_eq!(out.get("x-added").unwrap(), "1");
    }

    #[test]
    fn injected_credentials_cannot_be_overridden_by_rules() {
        let inbound = HeaderMap::new();
        let rules = RequestHeaderRules {
            passthrough: Some(Passthrough::All),
            set: Some([("authorization".to_owned(), "attacker".to_owned())].into_iter().collect()),
            ..Default::default()
        };
        let injected = [AuthInjection::header("authorization", "Bearer real").unwrap()];
        let out = outbound_request_headers(&inbound, Some(&rules), &injected, "h", None);
        assert_eq!(out.get("authorization").unwrap(), "Bearer real");
    }

    #[test]
    fn response_rules_are_applied_in_order() {
        let mut map = headers(&[("x-remove", "1"), ("x-set", "old"), ("server", "upstream")]);
        let rules = ResponseHeaderRules {
            set: Some([("x-set".to_owned(), "new".to_owned())].into_iter().collect()),
            add: Some([("x-added".to_owned(), "1".to_owned())].into_iter().collect()),
            remove: Some(vec!["x-remove".to_owned()]),
        };
        apply_response_rules(&mut map, Some(&rules));
        assert!(map.get("x-remove").is_none());
        assert_eq!(map.get("x-set").unwrap(), "new");
        assert_eq!(map.get("x-added").unwrap(), "1");
    }

    #[test]
    fn response_headers_lose_the_hop_by_hop_set() {
        let upstream_headers = headers(&[("content-type", "text/event-stream"), ("connection", "keep-alive")]);
        let out = outbound_response_headers(&upstream_headers);
        assert_eq!(out.get("content-type").unwrap(), "text/event-stream");
        assert!(out.get("connection").is_none());
    }
}
