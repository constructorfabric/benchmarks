//! Outbound and inbound header policy (DESIGN §3.2 header rules).
//!
//! The proxy never forwards hop-by-hop or routing headers, and never forwards
//! the inbound `Authorization` / `Cookie` headers unless the configuration
//! explicitly allows them — credentials must not leak across a trust boundary.

use http::header::{CONNECTION, COOKIE, HOST, TRANSFER_ENCODING};

use crate::domain::dto::{HeaderRules, Passthrough};

/// Headers that are consumed by the hop and must never be proxied.
pub const HOP_BY_HOP: [&str; 9] = [
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

/// Headers OAGW owns; the client may not dictate them.
pub const MANAGED: [&str; 6] = [
    "host",
    "content-length",
    "x-oagw-target-host",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
];

/// Headers that carry credentials and are only forwarded in `all` mode.
const CREDENTIAL: [&str; 2] = ["authorization", "cookie"];

/// Headers that must never surface on a gateway-generated error response.
pub const ERROR_RESPONSE_STRIPPED: [&str; 1] = ["set-cookie"];

/// Build the outbound header map from the inbound one.
#[must_use]
pub fn build_request_headers(
    inbound: &http::HeaderMap,
    rules: &HeaderRules,
    target_host: &str,
    target_port: u16,
    tls: bool,
    client_ip: Option<std::net::IpAddr>,
    default_user_agent: &str,
) -> http::HeaderMap {
    let mut outbound = http::HeaderMap::new();

    match rules.passthrough {
        Passthrough::None => {}
        Passthrough::Allowlist => {
            for name in &rules.passthrough_allowlist {
                if let Ok(header_name) = http::HeaderName::from_bytes(name.trim().as_bytes()) {
                    copy_all(inbound, &mut outbound, &header_name);
                }
            }
        }
        Passthrough::All => {
            for (name, value) in inbound.iter() {
                let lower = name.as_str().to_ascii_lowercase();
                if HOP_BY_HOP.contains(&lower.as_str())
                    || MANAGED.contains(&lower.as_str())
                    || CREDENTIAL.contains(&lower.as_str())
                {
                    continue;
                }
                append(&mut outbound, name, value);
            }
        }
    }

    // Headers named in `Connection` are hop-by-hop too (RFC 9110 §7.6.1).
    if let Some(connection) = inbound.get(CONNECTION).and_then(|v| v.to_str().ok()) {
        for token in connection.split(',') {
            let token = token.trim().to_ascii_lowercase();
            if !token.is_empty() {
                outbound.remove(&token);
            }
        }
    }
    for name in HOP_BY_HOP {
        outbound.remove(name);
    }
    for name in &rules.remove {
        let lower = name.trim().to_ascii_lowercase();
        if !lower.is_empty() {
            outbound.remove(&lower);
        }
    }
    apply_set_add(&mut outbound, rules);

    // Managed headers, always last so configuration cannot break the hop.
    outbound.remove(HOST);
    if let Ok(host_value) = http::HeaderValue::from_str(&format!("{target_host}:{target_port}")) {
        outbound.insert(HOST, host_value);
    }
    outbound.remove(TRANSFER_ENCODING);
    outbound.insert(
        "x-forwarded-proto",
        http::HeaderValue::from_static(if tls { "https" } else { "http" }),
    );
    if let Ok(value) = http::HeaderValue::from_str(&target_port.to_string()) {
        outbound.insert("x-forwarded-port", value);
    }
    if let Some(ip) = client_ip {
        // The client-supplied chain is preserved and the gateway's own view of
        // the peer is appended last (RFC 7239 §"forwarded" convention): the
        // upstream must trust the right-most entry, not the left-most one.
        for value in inbound.get_all("x-forwarded-for") {
            outbound.append("x-forwarded-for", value.clone());
        }
        if let Ok(value) = http::HeaderValue::from_str(&ip.to_string()) {
            outbound.append("x-forwarded-for", value);
        }
    }
    if !outbound.contains_key(http::header::USER_AGENT) {
        if let Ok(value) = http::HeaderValue::from_str(default_user_agent) {
            outbound.insert(http::header::USER_AGENT, value);
        }
    }
    outbound
}

/// The header passthrough applied to an upstream response.
///
/// DESIGN §"Headers Transformation" strips only *inbound* headers, and the
/// wire schema's `headers.response` object has no `passthrough` key: the
/// upstream's own response headers are part of the response the client asked
/// for, so they are forwarded (minus hop-by-hop). `set` / `add` / `remove`
/// from `headers.response` are applied on top of this baseline.
pub const RESPONSE_PASSTHROUGH: Passthrough = Passthrough::All;

/// Apply the response header rules to an upstream response.
#[must_use]
pub fn build_response_headers(
    upstream: &http::HeaderMap,
    rules: &HeaderRules,
    passthrough: Passthrough,
) -> http::HeaderMap {
    let mut outbound = http::HeaderMap::new();
    match passthrough {
        Passthrough::None => {}
        Passthrough::Allowlist => {
            for name in &rules.passthrough_allowlist {
                if let Ok(header_name) = http::HeaderName::from_bytes(name.trim().as_bytes()) {
                    copy_all(upstream, &mut outbound, &header_name);
                }
            }
        }
        Passthrough::All => {
            for (name, value) in upstream.iter() {
                let lower = name.as_str().to_ascii_lowercase();
                if HOP_BY_HOP.contains(&lower.as_str()) {
                    continue;
                }
                append(&mut outbound, name, value);
            }
        }
    }
    for name in HOP_BY_HOP {
        outbound.remove(name);
    }
    for name in &rules.remove {
        let lower = name.trim().to_ascii_lowercase();
        if !lower.is_empty() {
            outbound.remove(&lower);
        }
    }
    apply_set_add(&mut outbound, rules);
    outbound
}

/// Strip headers that must never surface on a gateway-generated error.
#[must_use]
pub fn sanitize_error_headers(mut headers: http::HeaderMap) -> http::HeaderMap {
    for name in ERROR_RESPONSE_STRIPPED {
        headers.remove(name);
    }
    headers
}

/// Stamp `X-OAGW-Error-Source` on a response (ADR-0007).
///
/// Every response leaving the proxy names its origin, so a client can tell a
/// gateway-generated problem apart from an upstream one even when the body is
/// not JSON.
pub fn stamp_error_source<B>(response: &mut http::Response<B>, source: &str) {
    if let Ok(value) = http::HeaderValue::from_str(source) {
        response.headers_mut().insert(
            http::HeaderName::from_static(crate::domain::error::ERROR_SOURCE_HEADER),
            value,
        );
    }
}

/// Headers that an upgrade request must carry on the outbound hop, even though
/// they are hop-by-hop and therefore stripped by [`build_request_headers`].
pub const UPGRADE_REQUEST_HEADERS: [&str; 6] = [
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// Headers of an upstream `101` that the client needs to see.
pub const UPGRADE_RESPONSE_HEADERS: [&str; 4] = [
    "connection",
    "upgrade",
    "sec-websocket-accept",
    "sec-websocket-protocol",
];

/// Re-attach the hop-by-hop headers an upgrade needs on the outbound hop.
///
/// [`build_request_headers`] removes them because a normal hop terminates them;
/// a tunneled upgrade is the exception (RFC 9110 §7.6.1, RFC 6455 §4.1). The
/// client's own values win, so `Sec-WebSocket-Key/Version/Protocol/Extensions`
/// negotiate end-to-end.
pub fn apply_upgrade_headers(outbound: &mut http::HeaderMap, inbound: &http::HeaderMap) {
    for name in UPGRADE_REQUEST_HEADERS {
        if !inbound.contains_key(name) {
            continue;
        }
        outbound.remove(name);
        for value in inbound.get_all(name) {
            outbound.append(name, value.clone());
        }
    }
    if !outbound.contains_key(http::header::CONNECTION) {
        outbound.insert(
            http::header::CONNECTION,
            http::HeaderValue::from_static("Upgrade"),
        );
    }
    if !outbound.contains_key(http::header::UPGRADE) {
        outbound.insert(
            http::header::UPGRADE,
            http::HeaderValue::from_static("websocket"),
        );
    }
}

/// Relay the negotiated upgrade handshake from the upstream `101` to the client.
#[must_use]
pub fn upgrade_response_headers(upstream: &http::HeaderMap) -> http::HeaderMap {
    let mut outbound = http::HeaderMap::new();
    for name in UPGRADE_RESPONSE_HEADERS {
        for value in upstream.get_all(name) {
            outbound.append(name, value.clone());
        }
    }
    if !outbound.contains_key(http::header::CONNECTION) {
        outbound.insert(
            http::header::CONNECTION,
            http::HeaderValue::from_static("Upgrade"),
        );
    }
    if !outbound.contains_key(http::header::UPGRADE) {
        outbound.insert(
            http::header::UPGRADE,
            http::HeaderValue::from_static("websocket"),
        );
    }
    outbound
}

/// `true` when the inbound header set carries an explicit credential header.
#[must_use]
pub fn has_client_credentials(headers: &http::HeaderMap) -> bool {
    headers.contains_key(http::header::AUTHORIZATION) || headers.contains_key(COOKIE)
}

fn apply_set_add(outbound: &mut http::HeaderMap, rules: &HeaderRules) {
    for (name, value) in &rules.set {
        if let (Ok(header_name), Ok(header_value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            outbound.insert(header_name, header_value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(header_name), Ok(header_value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            outbound.append(header_name, header_value);
        }
    }
}

fn copy_all(from: &http::HeaderMap, to: &mut http::HeaderMap, name: &http::HeaderName) {
    for value in from.get_all(name) {
        append(to, name, value);
    }
}

fn append(map: &mut http::HeaderMap, name: &http::HeaderName, value: &http::HeaderValue) {
    map.append(name.clone(), value.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                http::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                http::HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    fn send(inbound: &http::HeaderMap, rules: &HeaderRules) -> http::HeaderMap {
        build_request_headers(inbound, rules, "api.example.com", 443, true, None, "cf-gears-oagw")
    }

    #[test]
    fn passthrough_none_forwards_nothing_sensitive() {
        let inbound = headers(&[
            ("authorization", "Bearer sk-secret"),
            ("cookie", "session=1"),
            ("content-type", "application/json"),
            ("x-custom", "1"),
        ]);
        let outbound = send(&inbound, &HeaderRules::default());
        assert!(outbound.get("authorization").is_none());
        assert!(outbound.get("cookie").is_none());
        assert!(outbound.get("x-custom").is_none());
        assert_eq!(
            outbound.get("host").and_then(|v| v.to_str().ok()),
            Some("api.example.com:443")
        );
        assert_eq!(
            outbound.get("user-agent").and_then(|v| v.to_str().ok()),
            Some("cf-gears-oagw")
        );
    }

    #[test]
    fn passthrough_all_strips_hop_by_hop_and_managed() {
        let inbound = headers(&[
            ("connection", "x-drop-me"),
            ("x-drop-me", "1"),
            ("transfer-encoding", "chunked"),
            ("x-keep", "1"),
            ("content-length", "12"),
        ]);
        let rules = HeaderRules {
            passthrough: Passthrough::All,
            ..HeaderRules::default()
        };
        let outbound = send(&inbound, &rules);
        assert!(outbound.get("x-drop-me").is_none());
        assert!(outbound.get("transfer-encoding").is_none());
        assert!(outbound.get("content-length").is_none());
        assert_eq!(outbound.get("x-keep").and_then(|v| v.to_str().ok()), Some("1"));
    }

    #[test]
    fn allowlist_only_forwards_the_listed_headers() {
        let inbound = headers(&[("x-tenant", "a"), ("x-other", "b")]);
        let rules = HeaderRules {
            passthrough: Passthrough::Allowlist,
            passthrough_allowlist: vec![String::from("x-tenant")],
            ..HeaderRules::default()
        };
        let outbound = send(&inbound, &rules);
        assert_eq!(outbound.get("x-tenant").and_then(|v| v.to_str().ok()), Some("a"));
        assert!(outbound.get("x-other").is_none());
    }

    #[test]
    fn set_add_and_remove_are_applied() {
        let inbound = headers(&[("x-a", "1"), ("x-b", "1")]);
        let rules = HeaderRules {
            passthrough: Passthrough::All,
            set: [("x-a".to_owned(), "override".to_owned())].into_iter().collect(),
            add: [("x-c".to_owned(), "added".to_owned())].into_iter().collect(),
            remove: vec![String::from("x-b")],
            ..HeaderRules::default()
        };
        let outbound = send(&inbound, &rules);
        assert_eq!(outbound.get("x-a").and_then(|v| v.to_str().ok()), Some("override"));
        assert_eq!(outbound.get("x-c").and_then(|v| v.to_str().ok()), Some("added"));
        assert!(outbound.get("x-b").is_none());
    }

    #[test]
    fn client_ip_is_appended_to_x_forwarded_for() {
        let inbound = headers(&[("x-forwarded-for", "203.0.113.9")]);
        let outbound = build_request_headers(
            &inbound,
            &HeaderRules::default(),
            "api.example.com",
            443,
            true,
            Some(std::net::IpAddr::from([10u8, 0, 0, 1])),
            "cf-gears-oagw",
        );
        let values: Vec<&str> = outbound
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(values, vec!["203.0.113.9", "10.0.0.1"]);
    }

    #[test]
    fn response_headers_follow_the_same_rules() {
        let upstream = headers(&[("x-internal", "1"), ("set-cookie", "a=b"), ("x-public", "2")]);
        let outbound = build_response_headers(&upstream, &HeaderRules::default(), Passthrough::All);
        assert_eq!(outbound.get("x-public").and_then(|v| v.to_str().ok()), Some("2"));
        assert_eq!(
            outbound.get("x-internal").and_then(|v| v.to_str().ok()),
            Some("1")
        );
        let sanitized = sanitize_error_headers(outbound);
        assert!(sanitized.get("set-cookie").is_none());
    }

    #[test]
    fn client_credentials_are_detected() {
        assert!(has_client_credentials(&headers(&[("authorization", "Bearer x")])));
        assert!(!has_client_credentials(&headers(&[("x-other", "1")])));
    }

    // ── WebSocket upgrade headers (RFC 6455 §4.1, RFC 9110 §7.6.1) ─────────

    #[test]
    fn upgrade_headers_survive_the_hop_to_the_upstream() {
        let inbound = headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "chat, superchat"),
            ("sec-websocket-extensions", "permessage-deflate"),
        ]);
        let outbound = send(&inbound, &HeaderRules::default());
        // A normal hop strips them: they are hop-by-hop.
        assert!(outbound.get("upgrade").is_none());
        assert!(outbound.get("sec-websocket-key").is_none());

        // An upgrade re-attaches them with the client's own values.
        let mut tunneled = outbound.clone();
        apply_upgrade_headers(&mut tunneled, &inbound);
        for (name, value) in [
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "chat, superchat"),
            ("sec-websocket-extensions", "permessage-deflate"),
        ] {
            assert_eq!(
                tunneled.get(name).and_then(|v| v.to_str().ok()),
                if name == "sec-websocket-extensions" {
                    Some("permessage-deflate")
                } else {
                    Some(value)
                },
                "{name}"
            );
        }
        assert_eq!(
            tunneled.get("sec-websocket-key").and_then(|v| v.to_str().ok()),
            Some("dGhlIHNhbXBsZSBub25jZQ=="),
            "the client's own key negotiates the handshake"
        );
        assert!(tunneled.get("connection").is_some());
    }

    #[test]
    fn an_upgrade_without_client_headers_still_advertises_the_switch() {
        let inbound = headers(&[("content-type", "application/json")]);
        let mut outbound = send(&inbound, &HeaderRules::default());
        apply_upgrade_headers(&mut outbound, &inbound);
        assert_eq!(
            outbound.get("connection").and_then(|v| v.to_str().ok()),
            Some("Upgrade")
        );
        assert_eq!(
            outbound.get("upgrade").and_then(|v| v.to_str().ok()),
            Some("websocket")
        );
        assert!(outbound.get("sec-websocket-key").is_none());
    }

    #[test]
    fn upgrade_response_headers_reach_the_client() {
        let upstream = headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            ("sec-websocket-protocol", "chat"),
            ("date", "today"),
            ("server", "internal"),
        ]);
        let relayed = upgrade_response_headers(&upstream);
        assert_eq!(
            relayed
                .get("sec-websocket-accept")
                .and_then(|v| v.to_str().ok()),
            Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        assert_eq!(relayed.get("sec-websocket-protocol").and_then(|v| v.to_str().ok()), Some("chat"));
        assert!(relayed.get("server").is_none(), "no unrelated header leaks");
        assert_eq!(relayed.get("connection").and_then(|v| v.to_str().ok()), Some("Upgrade"));

        // A bare `101` still carries the two headers the client requires.
        let relayed = upgrade_response_headers(&headers(&[]));
        assert_eq!(relayed.get("connection").and_then(|v| v.to_str().ok()), Some("Upgrade"));
        assert_eq!(relayed.get("upgrade").and_then(|v| v.to_str().ok()), Some("websocket"));
    }
}
