//! Transform the Headers in Both Directions
//! (`cpt-cf-oagw-algo-proxy-transform-headers`).

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::model::upstream::{HeadersConfig, PassthroughMode};
use crate::proxy::constants::TARGET_HOST_HEADER;

/// Hop-by-hop headers unconditionally stripped in both directions, before
/// any configured rule runs (`inst-proxy-hdr-hop-strip`).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Response header this path additionally strips because the outbound HTTP
/// client (`toolkit_http::HttpClient`) transparently decompresses the
/// response body: forwarding the original `Content-Encoding` verbatim would
/// claim an encoding the relayed bytes no longer carry.
const CONTENT_ENCODING: &str = "content-encoding";

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
}

fn apply_remove(headers: &mut HeaderMap, remove: &[String]) {
    for name in remove {
        if let Ok(header_name) = HeaderName::try_from(name.as_str()) {
            headers.remove(header_name);
        }
    }
}

fn apply_set_add(
    headers: &mut HeaderMap,
    set: &std::collections::HashMap<String, String>,
    add: &std::collections::HashMap<String, String>,
) {
    for (name, value) in set {
        if let (Ok(header_name), Ok(header_value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(header_name, header_value);
        }
    }
    for (name, value) in add {
        if let (Ok(header_name), Ok(header_value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.append(header_name, header_value);
        }
    }
}

/// Request-direction header transform: strip routing + hop-by-hop headers,
/// apply the passthrough mode then `remove`/`set`/`add`, forward the
/// well-known content headers unconditionally, and force the connection
/// authority last so no configured rule can redirect it
/// (`inst-proxy-hdr-routing-strip` through `inst-proxy-hdr-content`).
// @cpt-algo:cpt-cf-oagw-algo-proxy-transform-headers:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-header-handling:p1
// @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-routing-strip
// @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-hop-strip
pub(crate) fn transform_request_headers(
    inbound: &HeaderMap,
    config: Option<&HeadersConfig>,
    authority: &str,
    content_length: Option<usize>,
    preserve_upgrade: bool,
) -> HeaderMap {
    let mut remaining = HeaderMap::new();
    for (name, value) in inbound {
        if name.as_str().eq_ignore_ascii_case(TARGET_HOST_HEADER) || is_hop_by_hop(name) {
            continue;
        }
        remaining.append(name.clone(), value.clone());
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-hop-strip
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-routing-strip

    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-passthrough
    let request_cfg = config.map(|c| &c.request);
    let mut out = HeaderMap::new();
    let content_type = remaining.get(axum::http::header::CONTENT_TYPE).cloned();
    match request_cfg.map(|r| r.passthrough).unwrap_or_default() {
        PassthroughMode::None => {}
        PassthroughMode::All => out = remaining,
        PassthroughMode::Allowlist => {
            let allowlist = request_cfg
                .map(|r| r.passthrough_allowlist.as_slice())
                .unwrap_or(&[]);
            for (name, value) in &remaining {
                if allowlist
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(name.as_str()))
                {
                    out.append(name.clone(), value.clone());
                }
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-passthrough

    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-request-rules
    if let Some(rules) = request_cfg {
        apply_remove(&mut out, &rules.remove);
        apply_set_add(&mut out, &rules.set, &rules.add);
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-request-rules

    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-content
    out.remove(axum::http::header::CONTENT_TYPE);
    if let Some(ct) = content_type {
        out.insert(axum::http::header::CONTENT_TYPE, ct);
    }
    out.remove(axum::http::header::CONTENT_LENGTH);
    if let Some(len) = content_length
        && let Ok(value) = HeaderValue::from_str(&len.to_string())
    {
        out.insert(axum::http::header::CONTENT_LENGTH, value);
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-content

    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-authority
    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-authority-final
    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-authority-not-routing
    out.remove(axum::http::header::HOST);
    if let Ok(value) = HeaderValue::from_str(authority) {
        // HTTP/2's `:authority` pseudo-header is not a regular `HeaderMap`
        // entry -- hyper derives it from the outbound request URI, which
        // this path always builds from the selected endpoint's authority,
        // so no separate action is needed for the HTTP/2 case here.
        out.insert(axum::http::header::HOST, value);
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-authority-not-routing
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-authority-final
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-authority

    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-upgrade-hook
    // DECOMPOSITION entry 2.6 (`cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`
    // `inst-stream-websocket-negotiation-02`): `Connection` and `Upgrade` are
    // both members of the hop-by-hop strip set above, and both are required
    // verbatim to complete a WebSocket handshake with the upstream. When the
    // caller identifies this request as an upgrade request, reinstate them
    // from the original `inbound` headers, bypassing the passthrough mode
    // and any configured `remove`/`set`/`add` rule -- exactly as the
    // unconditional `Host`/`Content-Type`/`Content-Length` handling above
    // does for its own headers. `Sec-WebSocket-*` headers need no such
    // exemption: they were never in the hop-by-hop strip set, so they
    // already pass through normally under the configured passthrough mode
    // above like any other request header.
    // @cpt-begin:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-02
    let mut out = out;
    if preserve_upgrade {
        if let Some(value) = inbound.get(axum::http::header::CONNECTION) {
            out.insert(axum::http::header::CONNECTION, value.clone());
        }
        if let Some(value) = inbound.get(axum::http::header::UPGRADE) {
            out.insert(axum::http::header::UPGRADE, value.clone());
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation:p2:inst-stream-websocket-negotiation-02
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-upgrade-hook

    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-scope
    // @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-return
    // The set/add/remove/passthrough vocabulary above is the whole
    // configured-rewrite surface; anything beyond it (conditional rewriting,
    // templating) is a transform plugin's concern, not this path's.
    out
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-return
    // @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-scope
}

/// Response-direction header transform: strip hop-by-hop (plus
/// `Content-Encoding`, since the client transparently decompresses), then
/// apply `remove`/`set`/`add` -- no passthrough mode exists for this
/// direction (`inst-proxy-hdr-response-rules`).
// @cpt-begin:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-response-rules
pub(crate) fn transform_response_headers(
    upstream: &HeaderMap,
    config: Option<&HeadersConfig>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream {
        if is_hop_by_hop(name) || name.as_str().eq_ignore_ascii_case(CONTENT_ENCODING) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Some(rules) = config.map(|c| &c.response) {
        apply_remove(&mut out, &rules.remove);
        apply_set_add(&mut out, &rules.set, &rules.add);
    }
    out
}
// @cpt-end:cpt-cf-oagw-algo-proxy-transform-headers:p2:inst-proxy-hdr-response-rules

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::upstream::RequestHeaderRules;
    use axum::http::HeaderValue;

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::try_from(*k).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn hop_by_hop_and_target_host_are_always_stripped() {
        let inbound = header_map(&[
            ("connection", "keep-alive"),
            ("keep-alive", "5"),
            ("proxy-authenticate", "x"),
            ("proxy-authorization", "x"),
            ("te", "trailers"),
            ("trailer", "x"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("x-oagw-target-host", "a.example.com"),
            ("x-app", "keep-me"),
        ]);
        let config = HeadersConfig {
            request: RequestHeaderRules {
                passthrough: PassthroughMode::All,
                ..Default::default()
            },
            ..Default::default()
        };
        let out =
            transform_request_headers(&inbound, Some(&config), "target.example.com", None, false);
        for name in HOP_BY_HOP {
            assert!(!out.contains_key(*name), "{name} should be stripped");
        }
        assert!(!out.contains_key("x-oagw-target-host"));
        assert!(out.contains_key("x-app"));
    }

    #[test]
    fn passthrough_none_drops_every_remaining_inbound_header() {
        let inbound = header_map(&[("x-app", "1"), ("authorization", "Bearer x")]);
        let out = transform_request_headers(&inbound, None, "target.example.com", None, false);
        assert!(!out.contains_key("x-app"));
        assert!(!out.contains_key("authorization"));
        assert!(out.contains_key("host"));
    }

    #[test]
    fn passthrough_allowlist_keeps_only_listed_names() {
        let inbound = header_map(&[("x-app", "1"), ("x-other", "2")]);
        let config = HeadersConfig {
            request: RequestHeaderRules {
                passthrough: PassthroughMode::Allowlist,
                passthrough_allowlist: vec!["x-app".to_owned()],
                ..Default::default()
            },
            ..Default::default()
        };
        let out =
            transform_request_headers(&inbound, Some(&config), "target.example.com", None, false);
        assert!(out.contains_key("x-app"));
        assert!(!out.contains_key("x-other"));
    }

    #[test]
    fn remove_then_set_then_add_apply_in_order() {
        let inbound = header_map(&[("x-app", "1")]);
        let mut set = std::collections::HashMap::new();
        set.insert("x-set".to_owned(), "s".to_owned());
        let mut add = std::collections::HashMap::new();
        add.insert("x-add".to_owned(), "a".to_owned());
        let config = HeadersConfig {
            request: RequestHeaderRules {
                passthrough: PassthroughMode::All,
                remove: vec!["x-app".to_owned()],
                set,
                add,
                ..Default::default()
            },
            ..Default::default()
        };
        let out =
            transform_request_headers(&inbound, Some(&config), "target.example.com", None, false);
        assert!(!out.contains_key("x-app"));
        assert_eq!(out.get("x-set").unwrap(), "s");
        assert_eq!(out.get("x-add").unwrap(), "a");
    }

    #[test]
    fn host_cannot_be_overridden_by_a_set_rule() {
        let inbound = header_map(&[("host", "caller.example.com")]);
        let mut set = std::collections::HashMap::new();
        set.insert("host".to_owned(), "attacker.example.com".to_owned());
        let config = HeadersConfig {
            request: RequestHeaderRules {
                passthrough: PassthroughMode::All,
                set,
                ..Default::default()
            },
            ..Default::default()
        };
        let out =
            transform_request_headers(&inbound, Some(&config), "target.example.com", None, false);
        assert_eq!(out.get("host").unwrap(), "target.example.com");
    }

    #[test]
    fn content_length_is_recomputed_and_content_type_forwarded_unchanged() {
        let inbound = header_map(&[
            ("content-type", "application/json"),
            ("content-length", "999"),
        ]);
        let out = transform_request_headers(&inbound, None, "target.example.com", Some(4), false);
        assert_eq!(out.get("content-type").unwrap(), "application/json");
        assert_eq!(out.get("content-length").unwrap(), "4");
    }

    /// `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`
    /// `inst-stream-websocket-negotiation-02`: when the caller marks this
    /// request as an upgrade, `Connection`/`Upgrade` survive verbatim even
    /// though they remain in the unconditional hop-by-hop strip set, and
    /// even under `PassthroughMode::None` (which would otherwise drop
    /// them along with every other header).
    #[test]
    fn preserve_upgrade_reinstates_connection_and_upgrade_under_passthrough_none() {
        let inbound = header_map(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("x-app", "dropped-under-none"),
        ]);
        let out = transform_request_headers(&inbound, None, "target.example.com", None, true);
        assert_eq!(out.get("connection").unwrap(), "Upgrade");
        assert_eq!(out.get("upgrade").unwrap(), "websocket");
        // `Sec-WebSocket-*` headers were never in the hop-by-hop strip set,
        // so they still obey the ordinary passthrough gate (`None` here)
        // and are absent, exactly as any other application header would be.
        assert!(!out.contains_key("sec-websocket-key"));
        assert!(!out.contains_key("x-app"));
    }

    /// Without the upgrade flag, `Connection`/`Upgrade` are stripped exactly
    /// as entry 2.5 always stripped them (the pre-2.6 behavior).
    #[test]
    fn upgrade_headers_stay_stripped_when_preserve_upgrade_is_false() {
        let inbound = header_map(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
        let out = transform_request_headers(&inbound, None, "target.example.com", None, false);
        assert!(!out.contains_key("connection"));
        assert!(!out.contains_key("upgrade"));
    }

    #[test]
    fn response_direction_strips_hop_by_hop_and_content_encoding() {
        let upstream = header_map(&[
            ("connection", "keep-alive"),
            ("content-encoding", "gzip"),
            ("x-app", "1"),
        ]);
        let out = transform_response_headers(&upstream, None);
        assert!(!out.contains_key("connection"));
        assert!(!out.contains_key("content-encoding"));
        assert!(out.contains_key("x-app"));
    }
}
