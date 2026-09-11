// Updated: 2026-09-01 by Constructor Tech
//! Header handling for the Data Plane.
//!
//! DESIGN's tables, in code. Three questions are answered here, each by its own
//! function: which inbound headers are forwarded, what the upstream sees as
//! `Host` and as the routing headers, and which response headers are passed
//! back to the caller.
//!
//! Hop-by-hop headers are never forwarded in either direction — they describe
//! the *connection*, not the *message*, and a connection is exactly what a
//! gateway terminates.

use crate::domain::dto::{PassthroughMode, RequestHeaderRules, ResponseHeaderRules};

/// Headers describing the transport, not the resource. Stripped from the
/// request before it is forwarded and from the response before it is returned.
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

/// Headers OAGW consumes itself. They select the upstream and the route, so
/// they are replaced rather than forwarded: an outbound `Host` naming the
/// *caller's* alias would be a lie to the upstream.
pub const ROUTING: &[&str] = &["host"];

/// Headers the gateway adds to every proxied exchange and therefore owns.
pub const GATEWAY: &[&str] = &["x-oagw-alias", "x-oagw-upstream-id"];

/// Whether a header name is stripped from a proxied request.
#[must_use]
pub fn is_stripped_from_request(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&name.as_str()) || ROUTING.contains(&name.as_str())
}

/// The policy for a request whose `passthrough` field is absent.
///
/// The upstream schema records `"default": "none"` for `passthrough`, and that
/// is the posture this implements: a gateway forwards what it is told to
/// forward, so the caller's own credentials and internal headers do not travel
/// to an external service unbidden. `content-type` is the exception, carried
/// separately below — DESIGN requires the gateway to "validate, set or adjust"
/// the well-known headers, and without it every proxied body arrives
/// unlabelled.
#[must_use]
fn allow_rule(mode: Option<PassthroughMode>) -> PassthroughMode {
    mode.unwrap_or(PassthroughMode::None)
}

/// The message headers the gateway carries itself, whatever the passthrough
/// policy. An explicit `set` or `remove` rule still wins over these.
const CARRIED: &[&str] = &["content-type"];

/// The headers a protocol upgrade cannot complete without, other than
/// `Upgrade` and `Connection` themselves.
const UPGRADE_HEADERS: &[&str] = &[
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// The protocol upgrade an inbound request asks for.
///
/// Returns the protocol (`websocket`) and the handshake headers the upstream
/// must see. `Upgrade` and `Connection` are hop-by-hop — they name *this*
/// connection, and the gateway is the one ending it — so the engine writes
/// fresh ones of its own; everything else the negotiation depends on is
/// collected here, whatever the passthrough policy says, because without them
/// the upstream answers the handshake with a 400 and no upgrade happens.
#[must_use]
pub fn requested_upgrade(inbound: &http::HeaderMap) -> Option<(String, http::HeaderMap)> {
    let is_upgrade = inbound
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.to_ascii_lowercase()
                .split(',')
                .any(|p| p.trim() == "upgrade")
        });
    let protocol = inbound
        .get(http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())?
        .trim()
        .to_ascii_lowercase();
    if !is_upgrade || protocol.is_empty() {
        return None;
    }

    let mut headers = http::HeaderMap::new();
    for name in UPGRADE_HEADERS {
        for value in inbound.get_all(*name) {
            if let Ok(parsed) = http::HeaderName::from_bytes(name.as_bytes()) {
                headers.append(parsed, value.clone());
            }
        }
    }
    Some((protocol, headers))
}

/// Build the header map sent upstream from the caller's request.
///
/// Applies, in order: the hop-by-hop and routing strips, the passthrough
/// policy, then `set`, then `add`.
#[must_use]
pub fn build_upstream_request(
    inbound: &http::HeaderMap,
    rules: &RequestHeaderRules,
    extra: impl Iterator<Item = (&'static str, String)>,
) -> http::HeaderMap {
    let mut out = http::HeaderMap::new();

    match allow_rule(rules.passthrough) {
        PassthroughMode::None => {}
        PassthroughMode::Allowlist => {
            for name in &rules.passthrough_allowlist {
                for value in inbound.get_all(name.as_str()) {
                    // `HeaderMap::append` only accepts a `&'static str` key, so
                    // the name is parsed into the owned form the map stores.
                    if let Ok(parsed) = http::HeaderName::from_bytes(name.as_bytes()) {
                        out.append(parsed, value.clone());
                    }
                }
            }
        }
        PassthroughMode::All => {
            for (name, value) in inbound {
                if is_stripped_from_request(name.as_str()) {
                    continue;
                }
                out.append(name, value.clone());
            }
        }
    }

    for name in CARRIED {
        if out.get(*name).is_none()
            && let Some(value) = inbound.get(*name)
            && let Ok(parsed) = http::HeaderName::from_bytes(name.as_bytes())
        {
            out.insert(parsed, value.clone());
        }
    }

    apply_set_add_remove(&mut out, Some(&rules.set), Some(&rules.add), &rules.remove);

    for (name, value) in extra {
        if let Ok(value) = http::HeaderValue::try_from(value) {
            out.insert(name, value);
        }
    }

    out
}

/// Build the header map returned to the caller from the upstream's response.
#[must_use]
pub fn build_client_response(
    upstream: &http::HeaderMap,
    rules: &ResponseHeaderRules,
) -> http::HeaderMap {
    let mut out = http::HeaderMap::new();
    for (name, value) in upstream {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        out.append(name, value.clone());
    }

    apply_set_add_remove(&mut out, Some(&rules.set), Some(&rules.add), &rules.remove);

    out
}

/// Apply a rules block: `set` replaces, `add` appends, `remove` deletes.
/// Unparsable names and values are dropped rather than failing the exchange —
/// a rule that cannot be represented in `http` is a configuration defect, and
/// DESIGN resolves those at create time.
fn apply_set_add_remove(
    out: &mut http::HeaderMap,
    set: Option<&std::collections::BTreeMap<String, String>>,
    add: Option<&std::collections::BTreeMap<String, String>>,
    remove: &[String],
) {
    for name in remove {
        if let Ok(name) = http::HeaderName::try_from(name.as_str()) {
            out.remove(name);
        }
    }
    if let Some(set) = set {
        for (name, value) in set {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::try_from(value.as_str()),
            ) {
                out.insert(name, value);
            }
        }
    }
    if let Some(add) = add {
        for (name, value) in add {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::try_from(value.as_str()),
            ) {
                out.append(name, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use std::collections::BTreeMap;

    fn rules(passthrough: Option<PassthroughMode>) -> RequestHeaderRules {
        RequestHeaderRules {
            set: BTreeMap::new(),
            add: BTreeMap::new(),
            remove: Vec::new(),
            passthrough,
            passthrough_allowlist: Vec::new(),
        }
    }

    fn inbound() -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        h.insert("content-type", HeaderValue::from_static("application/json"));
        h.insert("x-request-id", HeaderValue::from_static("caller-1"));
        h.insert("connection", HeaderValue::from_static("close"));
        h.insert("host", HeaderValue::from_static("api.example.com"));
        h
    }

    #[test]
    fn the_default_passthrough_forwards_nothing() {
        // The schema's `"default": "none"`: a gateway forwards what it is
        // told to, and the caller's credentials stay on the caller's side.
        let out = build_upstream_request(&inbound(), &rules(None), std::iter::empty());
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert!(out.get("x-request-id").is_none());
        assert!(out.get("connection").is_none());
        assert!(out.get("host").is_none());
    }

    #[test]
    fn an_explicit_all_passthrough_forwards_everything_but_the_strips() {
        let out = build_upstream_request(
            &inbound(),
            &rules(Some(PassthroughMode::All)),
            std::iter::empty(),
        );
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("x-request-id").unwrap(), "caller-1");
        assert!(out.get("connection").is_none());
        assert!(out.get("host").is_none());
    }

    #[test]
    fn a_set_rule_overrides_the_carried_content_type() {
        let mut r = rules(None);
        let mut set = BTreeMap::new();
        set.insert("content-type".to_owned(), "text/csv".to_owned());
        r.set = set;
        let out = build_upstream_request(&inbound(), &r, std::iter::empty());
        assert_eq!(out.get("content-type").unwrap(), "text/csv");
    }

    #[test]
    fn a_remove_rule_drops_the_carried_content_type() {
        let mut r = rules(None);
        r.remove = vec!["content-type".to_owned()];
        let out = build_upstream_request(&inbound(), &r, std::iter::empty());
        assert!(out.get("content-type").is_none());
    }

    #[test]
    fn none_passthrough_forwards_nothing_but_the_carried_headers() {
        let out = build_upstream_request(
            &inbound(),
            &rules(Some(PassthroughMode::None)),
            std::iter::empty(),
        );
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert!(out.get("x-request-id").is_none());
    }

    #[test]
    fn allowlist_passthrough_forwards_only_the_named() {
        let mut r = rules(Some(PassthroughMode::Allowlist));
        r.passthrough_allowlist = vec!["x-request-id".to_owned()];
        let out = build_upstream_request(&inbound(), &r, std::iter::empty());
        assert_eq!(out.get("x-request-id").unwrap(), "caller-1");
        // Nothing else the caller sent — content-type is carried by the
        // gateway itself, not by the allowlist.
        assert!(out.get("host").is_none());
        assert!(out.get("connection").is_none());
    }

    #[test]
    fn set_replaces_and_add_appends() {
        let mut r = rules(None);
        let mut set = BTreeMap::new();
        set.insert("x-api-version".to_owned(), "2024-01".to_owned());
        r.set = set;
        let mut add = BTreeMap::new();
        add.insert("x-trace".to_owned(), "t-1".to_owned());
        r.add = add;

        let mut inbound = inbound();
        inbound.insert("x-api-version", HeaderValue::from_static("old"));
        let out = build_upstream_request(&inbound, &r, std::iter::empty());
        assert_eq!(out.get("x-api-version").unwrap(), "2024-01");
        assert_eq!(out.get("x-trace").unwrap(), "t-1");
    }

    #[test]
    fn gateway_headers_are_always_set() {
        let out = build_upstream_request(
            &inbound(),
            &rules(None),
            [("x-oagw-alias", "api.example.com".to_owned())].into_iter(),
        );
        assert_eq!(out.get("x-oagw-alias").unwrap(), "api.example.com");
    }

    #[test]
    fn response_strips_hop_by_hop_only() {
        let mut upstream = http::HeaderMap::new();
        upstream.insert("content-type", "text/event-stream".parse().unwrap());
        upstream.insert("transfer-encoding", "chunked".parse().unwrap());
        upstream.insert("connection", "keep-alive".parse().unwrap());
        let out = build_client_response(&upstream, &ResponseHeaderRules::default());
        assert_eq!(out.get("content-type").unwrap(), "text/event-stream");
        assert!(out.get("transfer-encoding").is_none());
        assert!(out.get("connection").is_none());
    }

    fn upgrade_headers() -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        h.insert("connection", HeaderValue::from_static("Upgrade"));
        h.insert("upgrade", HeaderValue::from_static("websocket"));
        h.insert(
            "sec-websocket-key",
            HeaderValue::from_static("dGhlIHNhbXBsZQ=="),
        );
        h.insert("sec-websocket-version", HeaderValue::from_static("13"));
        h
    }

    #[test]
    fn a_websocket_upgrade_is_collected_with_its_handshake_headers() {
        let (protocol, headers) = requested_upgrade(&upgrade_headers()).expect("upgrade");
        assert_eq!(protocol, "websocket");
        assert_eq!(
            headers.get("sec-websocket-key").unwrap(),
            "dGhlIHNhbXBsZQ=="
        );
        assert_eq!(headers.get("sec-websocket-version").unwrap(), "13");
        // `Upgrade` and `Connection` name the caller's own connection; the
        // engine writes its own pair.
        assert!(headers.get("upgrade").is_none());
        assert!(headers.get("connection").is_none());
    }

    #[test]
    fn the_connection_token_is_matched_among_others() {
        let mut h = upgrade_headers();
        h.insert(
            "connection",
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        assert!(requested_upgrade(&h).is_some());
    }

    #[test]
    fn an_upgrade_without_the_connection_token_is_not_one() {
        let mut h = upgrade_headers();
        h.insert("connection", HeaderValue::from_static("keep-alive"));
        assert!(requested_upgrade(&h).is_none());
    }

    #[test]
    fn an_upgrade_that_names_no_protocol_is_not_one() {
        let mut h = upgrade_headers();
        h.remove("upgrade");
        assert!(requested_upgrade(&h).is_none());
    }

    #[test]
    fn a_plain_request_has_no_upgrade() {
        assert!(requested_upgrade(&inbound()).is_none());
    }
}
