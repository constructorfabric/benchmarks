//! Header transformation of the data plane (DESIGN.md §3.2).
//!
//! Three header families are handled:
//!
//! 1. **routing headers** — `X-OAGW-Target-Host` is consumed during routing
//!    and never forwarded;
//! 2. **hop-by-hop headers** — always stripped, plus every header the inbound
//!    `Connection` header names;
//! 3. **passthrough headers** — forwarded according to the upstream's
//!    `headers.request.passthrough` mode.
//!
//! `Host` is replaced by the selected endpoint's authority, and the
//! `headers.{request,response}` `set`/`add`/`remove` operations are applied
//! after the passthrough decision.

use axum::http::HeaderMap;
use axum::http::header;

use crate::domain::model::{PassthroughMode, RequestHeaderOps, ResponseHeaderOps};

/// Inbound header used to pick one endpoint of a pool; stripped before dialing.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Hop-by-hop headers stripped from both directions (DESIGN.md §3.2).
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

/// Inbound headers the data plane forwards when `passthrough` is `none`.
///
/// This is the "allowlist behaviour of the data plane" of
/// [`PassthroughMode::None`]: the standard headers a proxied API call needs —
/// content negotiation, credentials, forwarding and tracing — plus the
/// conditional-request headers. Everything else is dropped.
pub const DATAPLANE_ALLOWLIST: [&str; 21] = [
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "authorization",
    "cache-control",
    "content-type",
    "cookie",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-unmodified-since",
    "origin",
    "pragma",
    "range",
    "referer",
    "traceparent",
    "tracestate",
    "user-agent",
    "x-correlation-id",
    "x-request-id",
];

/// Headers the gateway owns: never forwarded, always recomputed.
const RECOMPUTED: [&str; 2] = ["host", "content-length"];

/// Strips the hop-by-hop headers and every header named by `Connection`.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let mut named: Vec<String> = Vec::new();
    for value in headers.get_all(header::CONNECTION) {
        if let Ok(raw) = value.to_str() {
            for token in raw.split(',') {
                let token = token.trim();
                if !token.is_empty() {
                    named.push(token.to_ascii_lowercase());
                }
            }
        }
    }
    let named = named.iter().map(String::as_str);
    for name in HOP_BY_HOP.iter().copied().chain(named) {
        remove_all(headers, name);
    }
}

/// Strips the routing header after it has been consumed by routing.
pub fn strip_routing_headers(headers: &mut HeaderMap) {
    remove_all(headers, TARGET_HOST_HEADER);
}

/// Strips the headers the gateway recomputes itself (`Host`,
/// `Content-Length`) so the outbound request carries exactly one of each.
pub fn strip_recomputed(headers: &mut HeaderMap) {
    for name in RECOMPUTED {
        remove_all(headers, name);
    }
}

fn remove_all(headers: &mut HeaderMap, name: &str) {
    if let Ok(parsed) = header::HeaderName::from_bytes(name.as_bytes()) {
        headers.remove(parsed);
    }
}

/// Builds the outbound request headers for a proxied call.
///
/// The inbound headers are filtered by `passthrough` (`none` keeps only
/// [`DATAPLANE_ALLOWLIST`], `allowlist` keeps only the configured names, `all`
/// keeps everything), then the routing, hop-by-hop and recomputed headers are
/// dropped, and finally the upstream's `set`/`add`/`remove` operations are
/// applied. The caller adds `Host` afterwards.
#[must_use]
pub fn outbound_request_headers(inbound: &HeaderMap, ops: Option<&RequestHeaderOps>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let mode = ops.map_or(PassthroughMode::None, |ops| ops.passthrough);
    for (name, value) in inbound {
        if forward(name.as_str(), mode, ops) {
            headers.append(name.clone(), value.clone());
        }
    }
    strip_routing_headers(&mut headers);
    strip_hop_by_hop(&mut headers);
    strip_recomputed(&mut headers);
    apply_request_ops(&mut headers, ops);
    headers
}

/// Restores the handshake headers a tunnelled upgrade needs.
///
/// [`outbound_request_headers`] strips `Connection` and `Upgrade` as
/// hop-by-hop headers (DESIGN.md §3.2) and drops the `Sec-WebSocket-*`
/// handshake fields, which are not on [`DATAPLANE_ALLOWLIST`]. An upgrade is
/// exactly the hop that negotiates them, so the caller's handshake is copied
/// back over the stripped outbound set (DESIGN.md §3.4); nothing else is
/// restored, so the passthrough policy still governs every other header.
#[must_use]
pub fn restore_upgrade_headers(
    outbound: Vec<(String, String)>,
    inbound: &HeaderMap,
) -> Vec<(String, String)> {
    let mut restored = outbound;
    for (name, value) in inbound {
        let name = name.as_str();
        let handshake = name.starts_with("sec-websocket")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("upgrade");
        if !handshake
            || restored
                .iter()
                .any(|(known, _)| known.eq_ignore_ascii_case(name))
            || name.eq_ignore_ascii_case("sec-websocket-accept")
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            restored.push((name.to_ascii_lowercase(), value.to_owned()));
        }
    }
    restored
}

/// Builds the response headers handed back to the caller: hop-by-hop headers
/// are dropped and `headers.response` operations are applied.
#[must_use]
pub fn outbound_response_headers(
    upstream: &HeaderMap,
    ops: Option<&ResponseHeaderOps>,
) -> Vec<(String, String)> {
    let mut headers = upstream.clone();
    strip_hop_by_hop(&mut headers);
    if let Some(ops) = ops {
        for name in &ops.remove {
            remove_all(&mut headers, &name.to_ascii_lowercase());
        }
        for (name, value) in &ops.set {
            if let (Ok(key), Ok(val)) = (
                header::HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()),
                header::HeaderValue::from_str(value),
            ) {
                headers.insert(key, val);
            }
        }
        for (name, value) in &ops.add {
            if let (Ok(key), Ok(val)) = (
                header::HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()),
                header::HeaderValue::from_str(value),
            ) {
                headers.append(key, val);
            }
        }
    }
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

/// Applies `headers.request` set/add/remove to an outbound header map.
pub fn apply_request_ops(headers: &mut HeaderMap, ops: Option<&RequestHeaderOps>) {
    let Some(ops) = ops else { return };
    for name in &ops.remove {
        remove_all(headers, &name.to_ascii_lowercase());
    }
    for (name, value) in &ops.set {
        if let (Ok(key), Ok(val)) = (
            header::HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()),
            header::HeaderValue::from_str(value),
        ) {
            headers.insert(key, val);
        }
    }
    for (name, value) in &ops.add {
        if let (Ok(key), Ok(val)) = (
            header::HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()),
            header::HeaderValue::from_str(value),
        ) {
            headers.append(key, val);
        }
    }
}

/// Whether `name` is forwarded under `mode`.
fn forward(name: &str, mode: PassthroughMode, ops: Option<&RequestHeaderOps>) -> bool {
    let lower = name.to_ascii_lowercase();
    match mode {
        PassthroughMode::All => true,
        PassthroughMode::None => DATAPLANE_ALLOWLIST.contains(&lower.as_str()),
        PassthroughMode::Allowlist => {
            ops.map(|ops| &ops.passthrough_allowlist)
                .is_some_and(|allowlist| {
                    allowlist
                        .iter()
                        .any(|entry| entry.trim().to_ascii_lowercase() == lower)
                })
        }
    }
}

/// Renders the `Host`/`:authority` value of an endpoint.
///
/// `port` is `None` for the scheme's standard port, in which case the
/// authority carries the host only.
#[must_use]
pub fn authority(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn request_ops(json: serde_json::Value) -> RequestHeaderOps {
        serde_json::from_value(json).expect("request header ops")
    }

    fn header_map(entries: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.insert(
                header::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        headers
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let mut headers = header_map(&[
            ("connection", "keep-alive, x-private"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("x-private", "nope"),
            ("accept", "application/json"),
        ]);
        strip_hop_by_hop(&mut headers);
        assert!(headers.get("connection").is_none());
        assert!(headers.get("keep-alive").is_none());
        assert!(headers.get("transfer-encoding").is_none());
        assert!(headers.get("upgrade").is_none());
        assert!(headers.get("x-private").is_none(), "Connection names it");
        assert!(headers.get("accept").is_some());
    }

    #[test]
    fn routing_headers_are_stripped() {
        let mut headers = header_map(&[("x-oagw-target-host", "b.example.com")]);
        strip_routing_headers(&mut headers);
        assert!(headers.get("x-oagw-target-host").is_none());
    }

    #[test]
    fn default_passthrough_keeps_the_dataplane_allowlist() {
        let inbound = header_map(&[
            ("authorization", "Bearer tok"),
            ("content-type", "application/json"),
            ("x-custom", "nope"),
            ("x-request-id", "req-1"),
        ]);
        let outbound = outbound_request_headers(&inbound, None);
        assert_eq!(
            outbound.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer tok")
        );
        assert!(outbound.get("x-custom").is_none(), "not on the allowlist");
        assert_eq!(
            outbound.get("x-request-id").and_then(|v| v.to_str().ok()),
            Some("req-1")
        );
        assert!(outbound.get("host").is_none());
        assert!(outbound.get("content-length").is_none());
    }

    #[test]
    fn allowlist_passthrough_forwards_only_the_named_headers() {
        let ops = request_ops(serde_json::json!({
            "passthrough": "allowlist",
            "passthrough_allowlist": ["X-Custom"],
        }));
        let inbound = header_map(&[
            ("x-custom", "kept"),
            ("authorization", "Bearer tok"),
            ("x-other", "dropped"),
        ]);
        let outbound = outbound_request_headers(&inbound, Some(&ops));
        assert_eq!(
            outbound.get("x-custom").and_then(|v| v.to_str().ok()),
            Some("kept")
        );
        assert!(outbound.get("authorization").is_none());
        assert!(outbound.get("x-other").is_none());
    }

    #[test]
    fn all_passthrough_forwards_everything_but_hop_by_hop() {
        let ops = request_ops(serde_json::json!({ "passthrough": "all" }));
        let inbound = header_map(&[
            ("x-custom", "kept"),
            ("connection", "close"),
            ("x-oagw-target-host", "a.example.com"),
        ]);
        let outbound = outbound_request_headers(&inbound, Some(&ops));
        assert_eq!(
            outbound.get("x-custom").and_then(|v| v.to_str().ok()),
            Some("kept")
        );
        assert!(outbound.get("connection").is_none());
        assert!(outbound.get("x-oagw-target-host").is_none());
    }

    #[test]
    fn request_operations_are_applied_in_order() {
        let ops = request_ops(serde_json::json!({
            "set": { "x-signed": "1" },
            "add": { "x-multi": "a" },
            "remove": ["x-drop"],
        }));
        let mut headers = header_map(&[("x-drop", "gone"), ("x-signed", "old")]);
        apply_request_ops(&mut headers, Some(&ops));
        assert_eq!(
            headers.get("x-signed").and_then(|v| v.to_str().ok()),
            Some("1")
        );
        assert!(headers.get("x-drop").is_none());
        assert_eq!(
            headers.get("x-multi").and_then(|v| v.to_str().ok()),
            Some("a")
        );
    }

    #[test]
    fn response_operations_are_applied() {
        let upstream = header_map(&[
            ("server", "mock"),
            ("x-secret", "gone"),
            ("connection", "keep-alive"),
        ]);
        let ops: ResponseHeaderOps = serde_json::from_value(serde_json::json!({
            "set": { "server": "oagw" },
            "add": { "x-added": "yes" },
            "remove": ["x-secret"],
        }))
        .expect("response header ops");
        let headers = outbound_response_headers(&upstream, Some(&ops));
        let value_of = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value_of("server").as_deref(), Some("oagw"));
        assert!(value_of("x-secret").is_none());
        assert!(value_of("connection").is_none());
        assert_eq!(value_of("x-added").as_deref(), Some("yes"));
    }

    #[test]
    fn authority_carries_the_explicit_port() {
        assert_eq!(authority("api.example.com", None), "api.example.com");
        assert_eq!(
            authority("api.example.com", Some(8080)),
            "api.example.com:8080"
        );
    }

    #[test]
    fn a_tunnel_keeps_the_handshake_the_proxy_path_strips() {
        let outbound = outbound_request_headers(
            &header_map(&[
                ("connection", "Upgrade"),
                ("upgrade", "websocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
                ("sec-websocket-protocol", "chat"),
                ("origin", "https://app.example.com"),
                ("authorization", "Bearer token"),
                ("x-private", "nope"),
            ]),
            None,
        );
        let restored = restore_upgrade_headers(
            outbound
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_owned(), value.to_owned()))
                })
                .collect(),
            &header_map(&[
                ("connection", "Upgrade"),
                ("upgrade", "WebSocket"),
                ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
                ("sec-websocket-version", "13"),
                ("sec-websocket-protocol", "chat"),
                ("origin", "https://app.example.com"),
                ("authorization", "Bearer token"),
                ("x-private", "nope"),
            ]),
        );
        let value_of = |name: &str| {
            restored
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value_of("connection").as_deref(), Some("Upgrade"));
        assert_eq!(value_of("upgrade").as_deref(), Some("WebSocket"));
        assert_eq!(
            value_of("sec-websocket-key").as_deref(),
            Some("dGhlIHNhbXBsZSBub25jZQ==")
        );
        assert_eq!(value_of("sec-websocket-version").as_deref(), Some("13"));
        assert_eq!(value_of("sec-websocket-protocol").as_deref(), Some("chat"));
        // The passthrough policy still governs every other header.
        assert_eq!(value_of("authorization").as_deref(), Some("Bearer token"));
        assert!(value_of("x-private").is_none(), "not a handshake header");
    }

    #[test]
    fn a_tunnel_does_not_reinvent_the_accept_header() {
        let inbound = header_map(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-accept", "s3pPLmBv6XNfFUTGmxt0tf9eGuU="),
        ]);
        let restored =
            restore_upgrade_headers(vec![("host".to_owned(), "upstream".to_owned())], &inbound);
        assert!(
            !restored
                .iter()
                .any(|(name, _)| name == "sec-websocket-accept"),
            "the accept header is computed by the upstream, never forwarded"
        );
        assert!(restored.iter().any(|(name, _)| name == "host"));
    }
}
