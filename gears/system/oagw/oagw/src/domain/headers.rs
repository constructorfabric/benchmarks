//! Header transformation for proxied requests and responses (DESIGN §3.5
//! "Headers Transformation").
//!
//! Two properties are load-bearing here and both are security properties:
//!
//! * **Hop-by-hop isolation** — RFC 9110 §7.6.1 headers (and every header the
//!   `Connection` header names) describe one connection only; forwarding them
//!   to another connection corrupts framing or leaks proxy credentials.
//! * **No implicit credential forwarding** — `Authorization` and `Cookie` are
//!   the caller's credentials for *this* gateway. With the default
//!   [`PassthroughMode::None`](crate::domain::types::PassthroughMode) they are
//!   dropped; they reach the upstream only when the operator asks for it
//!   (`passthrough: all`) or names them in the allowlist.
//!
//! `X-OAGW-Target-Host` is gateway-internal routing state and is stripped in
//! every mode.

use http::{HeaderMap, HeaderName, HeaderValue, Method};

use crate::domain::types::{Endpoint, HeaderTransform, HeaderTransformResponse, PassthroughMode};
use crate::error::OagwError;

/// RFC 9110 §7.6.1 hop-by-hop headers, plus `Host` which the proxy recomputes.
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Inbound headers the gateway derives itself and therefore never forwards
/// from the request.
///
/// Two families, both gateway-owned response state:
///
/// * the framing and routing headers the proxy recomputes per hop (`host`,
///   `content-length`, `X-OAGW-Target-Host`);
/// * the budget and CORS headers the gateway stamps on its own responses
///   (ADR-0003 "Response Headers", ADR-0004), which a caller has no business
///   supplying: under `passthrough: all` a forwarded `X-RateLimit-Remaining`
///   or `Access-Control-Allow-Origin` would reach the upstream verbatim and
///   stand in for a value the gateway never computed.
///
/// `Authorization` and `Cookie` are deliberately **not** here: they are
/// governed by [`is_credential`] and the passthrough policy, so `passthrough:
/// all` or an allowlist entry can forward them.
pub const GATEWAY_OWNED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    crate::domain::routing::TARGET_HOST_HEADER,
    // Rate-limit budget state (ADR-0003 "Response Headers").
    "x-ratelimit-limit",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    // CORS answers (ADR-0004), on both sides of the conversation: the
    // `Access-Control-Request-*` pair belongs to a preflight, which the gateway
    // answers itself and never forwards.
    "access-control-allow-origin",
    "access-control-allow-methods",
    "access-control-allow-headers",
    "access-control-expose-headers",
    "access-control-max-age",
    "access-control-allow-credentials",
    "access-control-request-method",
    "access-control-request-headers",
    // A response header; a request value for it means nothing to the upstream.
    "vary",
];

/// Whether `name` is a hop-by-hop header.
#[must_use]
pub fn is_hop_by_hop(name: &HeaderName) -> bool {
    header_in(name.as_str(), HOP_BY_HOP_HEADERS)
}

/// Whether `name` is a header the gateway sets itself.
#[must_use]
pub fn is_gateway_owned(name: &HeaderName) -> bool {
    header_in(name.as_str(), GATEWAY_OWNED_HEADERS)
}

/// The `Authorization` / `Cookie` credentials, which are never forwarded by
/// default.
#[must_use]
pub fn is_credential(name: &HeaderName) -> bool {
    name.as_str() == "authorization" || name.as_str() == "cookie"
}

fn header_in(name: &str, list: &[&str]) -> bool {
    list.iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// Names of the headers listed in the `Connection` header.
///
/// RFC 9110 §7.6.1: a header named there is hop-by-hop even when its own name
/// is not in the static list.
#[must_use]
pub fn connection_named(headers: &HeaderMap) -> Vec<HeaderName> {
    let mut named = Vec::new();
    for value in headers.get_all("connection") {
        let Ok(value) = value.to_str() else { continue };
        for token in value.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Ok(name) = HeaderName::from_lowercase(token.to_ascii_lowercase().as_bytes())
                && !named.contains(&name)
            {
                named.push(name);
            }
        }
    }
    named
}

/// Remove every hop-by-hop header from `headers` in place.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named = connection_named(headers);
    let drop: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_hop_by_hop(name) || named.contains(name))
        .cloned()
        .collect();
    for name in drop {
        headers.remove(&name);
    }
}

/// The WebSocket handshake headers that are forwarded from the caller to the
/// upstream **verbatim** (RFC 6455 §4.1).
///
/// `Sec-WebSocket-Key` is the client's contribution to the handshake the
/// gateway signs; rewriting it would produce a `Sec-WebSocket-Accept` the
/// caller cannot verify. `Sec-WebSocket-Version` and `Sec-WebSocket-Protocol`
/// are the caller's offer, and only the upstream may pick from it.
///
/// They are deliberately **not** in [`GATEWAY_OWNED_HEADERS`] — the gateway does
/// not derive them — and not credentials under [`is_credential`]: a handshake
/// key is not a secret (RFC 6455 §1.3, "the key is a base64-encoded 16-byte
/// value chosen randomly" whose only purpose is to defeat a naive intermediary),
/// so it must survive the inbound strip and is never treated as caller
/// credential material.
const WEBSOCKET_HANDSHAKE_HEADERS: &[&str] = &[
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
];

/// The one WebSocket header the gateway must never forward or echo back.
///
/// The splice below the 101 has no extension support (tungstenite negotiates
/// none), so an extension the upstream agreed to would corrupt both legs: the
/// upstream would send `permessage-deflate` frames the gateway would relay
/// byte-for-byte to a client that was never told to expect them.
pub const WEBSOCKET_EXTENSIONS_HEADER: &str = "sec-websocket-extensions";

/// Whether `headers` carries an HTTP/1.1 WebSocket upgrade (RFC 6455 §4.1).
///
/// One rule, owned by the gateway and used by the transport: `GET`, a
/// `Connection` header whose token list contains `upgrade`, and an `Upgrade`
/// header whose value is `websocket` — all case-insensitive. `Upgrade` is read
/// across *all* its values (RFC 9110 §7.9 allows a sender to repeat the field),
/// because `connection_named` already scans every `Connection` value and a
/// second `Upgrade` after a first one that names something else would otherwise
/// hide the handshake. Anything else falls through to the ordinary proxy path,
/// where the header policy strips `Connection` and `Upgrade` as the hop-by-hop
/// headers they are.
///
/// The HTTP/1.1 version gate is not part of this rule: it is the extractor's
/// ([`axum::extract::ws::WebSocketUpgrade`], which reads `parts.version`), and a
/// handshake this rule detects on a version it does not support is answered with
/// the extractor's rejection, not forwarded.
///
/// # Documented deviation
///
/// Only HTTP/1.1 is detected. An HTTP/2 WebSocket arrives as an extended
/// `CONNECT` with a `:protocol` pseudo-header instead of an `Upgrade` header
/// (RFC 9113 §8.5); this gateway build neither parses a `:protocol` request nor
/// has an HTTP/2 client path that yields a duplex byte stream to splice, so such
/// a request is routed as an ordinary `CONNECT` and answered by whatever the
/// matched route decides. PRD §5.4 names WebTransport as well: `wt` is a scheme
/// on the control-plane types but no transport exists for it in this slice.
#[must_use]
pub fn is_websocket_upgrade(method: &Method, headers: &HeaderMap) -> bool {
    // `Connection` and `Upgrade` are hop-by-hop, so they are meaningful only on
    // the one connection that carries them; a proxy *must* consume them rather
    // than forward them blindly (RFC 9110 §7.6.1) — which is why the detection
    // lives next to the hop-by-hop policy it overrides.
    if method != Method::GET {
        return false;
    }

    if !connection_named(headers)
        .iter()
        .any(|name| name.as_str() == "upgrade")
    {
        return false;
    }

    headers
        .get_all(http::header::UPGRADE)
        .iter()
        .any(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

/// Put the caller's WebSocket handshake back onto the outbound header set.
///
/// [`transform_request_headers`] strips `Connection` and `Upgrade` (they are
/// hop-by-hop) and — under the default `passthrough: none` — every other inbound
/// header too. A handshake needs exactly the opposite of a body-carrying
/// request: hyper's client only takes the upgrade path when *both* `Connection:
/// Upgrade` and `Upgrade: websocket` are on the outbound request, and the
/// upstream cannot complete the handshake without the caller's
/// `Sec-WebSocket-*` headers. They are therefore re-instated *after* the
/// passthrough policy and the operator's rules, so that no configuration can
/// silently break a handshake it did not think about.
///
/// `Connection` and `Upgrade` are re-emitted canonically rather than copied: the
/// caller may have listed other tokens (`keep-alive`) next to `upgrade`, and
/// every one of them is hop-by-hop state of a connection that is about to stop
/// being an HTTP connection at all.
pub fn restore_websocket_handshake(outbound: &mut HeaderMap, inbound: &HeaderMap) {
    outbound.insert(
        http::header::CONNECTION,
        HeaderValue::from_static("Upgrade"),
    );
    outbound.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));

    for name in WEBSOCKET_HANDSHAKE_HEADERS {
        let name = HeaderName::from_static(name);
        outbound.remove(&name);
        for value in inbound.get_all(&name) {
            outbound.append(&name, value.clone());
        }
    }

    // The gateway never negotiated an extension, so none must be offered either.
    outbound.remove(WEBSOCKET_EXTENSIONS_HEADER);
    // A handshake carries no body, and no framing for one (RFC 6455 §4.1: the
    // request "MUST NOT carry a body").
    outbound.remove(http::header::CONTENT_LENGTH);
    outbound.remove(http::header::TRANSFER_ENCODING);
}

/// Remove the framing the gateway's own HTTP stack owns on a response that
/// switches protocols.
///
/// A `101 Switching Protocols` has no body and no framing for one: what the
/// upstream said about `Content-Length` describes bytes that will never be sent,
/// and hyper decides the framing of the response it writes itself. The
/// hop-by-hop headers (`connection`, `upgrade`, `transfer-encoding`,
/// `keep-alive`) are already gone through [`transform_response_headers`]; what
/// is left here is the length framing and [`WEBSOCKET_EXTENSIONS_HEADER`], which
/// the gateway must never echo (see [`restore_websocket_handshake`]).
pub fn strip_switched_protocol_framing(headers: &mut HeaderMap) {
    headers.remove(http::header::CONTENT_LENGTH);
    headers.remove(http::header::TRANSFER_ENCODING);
    headers.remove(WEBSOCKET_EXTENSIONS_HEADER);
}

/// The `Host` authority of `endpoint`: `host`, or `host:port` for a
/// non-standard port (DESIGN §3.5 "Headers Transformation").
///
/// An IPv6 endpoint is always bracketed: `[::1]:8080` is the only form both
/// `http::Uri`'s authority parser and the `Host` header accept for an IPv6
/// literal — the unbracketed `::1:8080` is a multi-colon authority and is
/// rejected outright. `validate_host` stores the *unbracketed* form, so the
/// brackets are added back here, at the two places an authority is rendered:
/// the outbound request URI and the `Host` header.
#[must_use]
pub fn host_authority(endpoint: &Endpoint) -> String {
    let host = host_token(&endpoint.host);
    match endpoint.port == endpoint.scheme.standard_port() {
        true => host,
        false => format!("{host}:{}", endpoint.port),
    }
}

/// The host as it appears inside an authority: IPv6 literals in brackets.
fn host_token(host: &str) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// Build the header map forwarded to `endpoint`.
///
/// The inbound headers are filtered by the passthrough policy, then the
/// configured `set` / `add` / `remove` rules are applied, then `Host` is
/// replaced with the endpoint authority. A rule that explicitly `set`s `Host`
/// therefore wins over the endpoint authority. `content_length` is the length
/// of the body actually forwarded, so the framing header always matches the
/// bytes on the wire.
///
/// Header names in the configuration that are not valid RFC 9110 names are
/// ignored rather than failing the request: the proxy degrades to the
/// operator's remaining rules.
#[must_use]
pub fn transform_request_headers(
    inbound: &HeaderMap,
    transform: Option<&HeaderTransform>,
    endpoint: &Endpoint,
    content_length: Option<u64>,
) -> HeaderMap {
    let mut outbound = filter_inbound(inbound, transform);

    if let Some(transform) = transform {
        apply_set(&mut outbound, &transform.set);
        apply_add(&mut outbound, &transform.add);
        apply_remove(&mut outbound, &transform.remove);
    }

    // `Host` is replaced with the endpoint authority unless the operator's
    // `set` rules already pinned it (a configured override wins).
    if !outbound.contains_key(http::header::HOST) {
        let authority = host_authority(endpoint);
        let host = HeaderValue::from_str(&authority).unwrap_or_else(|_| {
            // `validate_host` already guarantees a wire-safe host, and the port
            // is numeric, so this branch is unreachable; keep the request well
            // formed anyway.
            HeaderValue::from_static("invalid-host")
        });
        outbound.insert(http::header::HOST, host);
    }

    if let Some(length) = content_length
        && let Ok(value) = HeaderValue::from_str(&length.to_string())
    {
        outbound.insert(http::header::CONTENT_LENGTH, value);
    }

    outbound
}

fn filter_inbound(inbound: &HeaderMap, transform: Option<&HeaderTransform>) -> HeaderMap {
    let (passthrough, allowlist) = match transform {
        Some(transform) => (
            transform.passthrough,
            transform.passthrough_allowlist.as_slice(),
        ),
        None => (PassthroughMode::None, &[] as &[String]),
    };

    let named = connection_named(inbound);
    let mut outbound = HeaderMap::with_capacity(inbound.len());

    if passthrough == PassthroughMode::None {
        return outbound;
    }

    for (name, value) in inbound {
        if is_hop_by_hop(name) || named.contains(name) || is_gateway_owned(name) {
            continue;
        }
        if is_credential(name)
            && passthrough != PassthroughMode::All
            && !allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name.as_str()))
        {
            continue;
        }
        if passthrough == PassthroughMode::Allowlist
            && !allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name.as_str()))
        {
            continue;
        }
        outbound.append(name.clone(), value.clone());
    }

    outbound
}

fn apply_set(outbound: &mut HeaderMap, rules: &std::collections::BTreeMap<String, String>) {
    for (name, value) in rules {
        let Ok(name) = HeaderName::from_lowercase(name.to_ascii_lowercase().as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_str(value) {
            outbound.insert(name, value);
        }
    }
}

fn apply_add(outbound: &mut HeaderMap, rules: &std::collections::BTreeMap<String, String>) {
    for (name, value) in rules {
        let Ok(name) = HeaderName::from_lowercase(name.to_ascii_lowercase().as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_str(value) {
            outbound.append(name, value);
        }
    }
}

fn apply_remove(outbound: &mut HeaderMap, names: &[String]) {
    for name in names {
        if let Ok(name) = HeaderName::from_lowercase(name.to_ascii_lowercase().as_bytes()) {
            outbound.remove(&name);
        }
    }
}

/// Apply the response rules to an upstream response's headers in place.
///
/// Hop-by-hop headers are removed first, then `set` / `add` / `remove`.
pub fn transform_response_headers(
    headers: &mut HeaderMap,
    transform: Option<&HeaderTransformResponse>,
) {
    strip_hop_by_hop(headers);

    let Some(transform) = transform else {
        return;
    };

    for (name, value) in &transform.set {
        let Ok(name) = HeaderName::from_lowercase(name.to_ascii_lowercase().as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &transform.add {
        let Ok(name) = HeaderName::from_lowercase(name.to_ascii_lowercase().as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.append(name, value);
        }
    }
    for name in &transform.remove {
        if let Ok(name) = HeaderName::from_lowercase(name.to_ascii_lowercase().as_bytes()) {
            headers.remove(&name);
        }
    }
}

/// Headers added to every response the gateway produces itself.
///
/// `X-OAGW-Error-Source: gateway` is the ADR-0007 marker that tells the caller
/// the body is a gateway problem, not an upstream payload.
#[must_use]
pub fn gateway_error_headers() -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(2);
    headers.insert(
        crate::error::error_source_header_name(),
        crate::error::error_source_header_value(crate::error::ERROR_SOURCE_GATEWAY),
    );
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static(crate::error::PROBLEM_JSON_MEDIA_TYPE),
    );
    headers
}

/// The error a non-`http` endpoint produces (no TLS client in this slice).
///
/// # Errors
/// Always [`OagwErrorKind::DownstreamError`](crate::error::OagwErrorKind).
pub fn unsupported_scheme_error(scheme: crate::domain::types::Scheme) -> OagwError {
    OagwError::downstream_error(format!(
        "endpoint scheme `{}` cannot be dialed by this gateway build; only `http` endpoints can \
         be forwarded",
        scheme_name(scheme)
    ))
    .with_extension("scheme", serde_json::json!(scheme_name(scheme)))
}

/// The error an `http` endpoint produces while the configuration forbids
/// plaintext upstreams (`allow_http_upstream: false`, DESIGN §2.2).
///
/// Same 502 family as [`unsupported_scheme_error`]: the endpoint cannot be
/// dialed, but here the block is a *configuration* decision, so the detail
/// names the setting that made it.
///
/// # Errors
/// Always [`OagwErrorKind::DownstreamError`](crate::error::OagwErrorKind).
pub fn http_scheme_not_allowed_error() -> OagwError {
    OagwError::downstream_error(
        "endpoint scheme `http` is blocked by the configuration (`allow_http_upstream: false`); \
         only `https` endpoints can be forwarded",
    )
    .with_extension("scheme", serde_json::json!("http"))
    .with_extension("allow_http_upstream", serde_json::json!(false))
}

/// The lowercase wire name of a scheme.
fn scheme_name(scheme: crate::domain::types::Scheme) -> &'static str {
    match scheme {
        crate::domain::types::Scheme::Http => "http",
        crate::domain::types::Scheme::Https => "https",
        crate::domain::types::Scheme::Wss => "wss",
        crate::domain::types::Scheme::Wt => "wt",
        crate::domain::types::Scheme::Grpc => "grpc",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::Scheme;
    use std::collections::BTreeMap;

    fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            let name = HeaderName::from_lowercase(name.as_bytes()).expect("valid name");
            map.append(name, HeaderValue::from_str(value).expect("valid value"));
        }
        map
    }

    fn names(headers: &HeaderMap) -> Vec<String> {
        let mut names: Vec<String> = headers
            .keys()
            .map(|name| name.as_str().to_owned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The first value of `name` as text, for assertions.
    fn str_of<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|value| value.to_str().ok())
    }

    fn transform() -> HeaderTransform {
        HeaderTransform {
            set: BTreeMap::new(),
            add: BTreeMap::new(),
            remove: Vec::new(),
            passthrough: PassthroughMode::None,
            passthrough_allowlist: Vec::new(),
        }
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let mut headers = map(&[
            ("connection", "keep-alive, x-tracing"),
            ("keep-alive", "timeout=5"),
            ("x-tracing", "abc"),
            ("te", "trailers"),
            ("upgrade", "websocket"),
            ("transfer-encoding", "chunked"),
            ("proxy-authorization", "Basic zzz"),
            ("x-keep", "yes"),
        ]);

        strip_hop_by_hop(&mut headers);

        assert_eq!(names(&headers), vec!["x-keep"]);
    }

    #[test]
    fn the_gateway_owned_response_headers_are_stripped_in_every_passthrough_mode() {
        // A caller that could supply these would be writing the gateway's
        // response for it: a budget never spent, a CORS answer never given, a
        // cache key that never varied (ADR-0003 "Response Headers", ADR-0004).
        let owned: &[&str] = &[
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
            "access-control-allow-origin",
            "access-control-allow-methods",
            "access-control-allow-headers",
            "access-control-expose-headers",
            "access-control-max-age",
            "access-control-allow-credentials",
            "access-control-request-method",
            "access-control-request-headers",
            "vary",
        ];

        for passthrough in [
            PassthroughMode::All,
            PassthroughMode::Allowlist,
            PassthroughMode::None,
        ] {
            let mut inbound = map(&[("x-caller-header", "mine")]);
            for name in owned {
                let name = HeaderName::from_lowercase(name.as_bytes()).expect("a valid name");
                inbound.insert(name, HeaderValue::from_static("smuggled"));
            }

            let transform = HeaderTransform {
                passthrough,
                ..transform()
            };
            let outbound = transform_request_headers(
                &inbound,
                Some(&transform),
                &endpoint(Scheme::Http, "10.0.0.1", 8080),
                Some(0),
            );

            for name in owned {
                assert!(
                    outbound.get(*name).is_none(),
                    "{passthrough:?}: the caller cannot supply {name}"
                );
            }
            // `none` forwards nothing at all, and `allowlist` drops the unlisted
            // caller header for its own reason; only `all` is expected to keep it.
            match passthrough {
                PassthroughMode::All => assert_eq!(
                    str_of(&outbound, "x-caller-header"),
                    Some("mine"),
                    "nothing but the gateway-owned names is dropped"
                ),
                _ => assert_eq!(str_of(&outbound, "x-caller-header"), None),
            }
        }
    }

    #[test]
    fn target_host_is_always_stripped() {
        let inbound = map(&[
            ("x-oagw-target-host", "us.vendor.com"),
            ("accept", "application/json"),
        ]);
        let transform = transform();
        let endpoint = endpoint(Scheme::Http, "10.0.0.1", 8080);

        let outbound = transform_request_headers(&inbound, Some(&transform), &endpoint, Some(0));

        assert!(
            outbound.get("x-oagw-target-host").is_none(),
            "internal routing state must not reach the upstream"
        );
        assert_eq!(str_of(&outbound, "host"), Some("10.0.0.1:8080"));
        let _ = inbound;
    }

    #[test]
    fn default_passthrough_forwards_no_inbound_header() {
        let inbound = map(&[
            ("authorization", "Bearer secret"),
            ("cookie", "session=1"),
            ("x-request-id", "abc"),
            ("x-oagw-target-host", "us.vendor.com"),
        ]);
        let endpoint = endpoint(Scheme::Https, "api.openai.com", 443);

        let outbound = transform_request_headers(&inbound, None, &endpoint, Some(7));

        assert_eq!(names(&outbound), vec!["content-length", "host"]);
        assert_eq!(str_of(&outbound, "host"), Some("api.openai.com"));
        assert_eq!(str_of(&outbound, "content-length"), Some("7"));
    }

    #[test]
    fn credentials_are_only_forwarded_when_the_allowlist_names_them() {
        let inbound = map(&[
            ("authorization", "Bearer secret"),
            ("cookie", "session=1"),
            ("x-request-id", "abc"),
        ]);
        let endpoint = endpoint(Scheme::Https, "api.openai.com", 443);

        // Allowlist without credentials.
        let mut rules = transform();
        rules.passthrough = PassthroughMode::Allowlist;
        rules.passthrough_allowlist = vec!["x-request-id".to_owned()];
        let outbound = transform_request_headers(&inbound, Some(&rules), &endpoint, None);
        assert_eq!(names(&outbound), vec!["host", "x-request-id"]);

        // Allowlist naming Authorization only forwards that one.
        rules.passthrough_allowlist = vec!["Authorization".to_owned()];
        let outbound = transform_request_headers(&inbound, Some(&rules), &endpoint, None);
        assert_eq!(names(&outbound), vec!["authorization", "host"]);
        assert_eq!(str_of(&outbound, "authorization"), Some("Bearer secret"));
    }

    #[test]
    fn passthrough_all_forwards_everything_but_hop_by_hop_and_credentials_rules_apply() {
        let inbound = map(&[
            ("authorization", "Bearer secret"),
            ("x-request-id", "abc"),
            ("transfer-encoding", "chunked"),
            ("x-oagw-target-host", "us.vendor.com"),
        ]);
        let mut rules = transform();
        rules.passthrough = PassthroughMode::All;
        let endpoint = endpoint(Scheme::Http, "10.0.0.1", 80);

        let outbound = transform_request_headers(&inbound, Some(&rules), &endpoint, None);

        assert_eq!(
            names(&outbound),
            vec!["authorization", "host", "x-request-id"]
        );
    }

    #[test]
    fn set_add_and_remove_apply_in_that_order() {
        let inbound = map(&[("x-a", "inbound"), ("x-b", "inbound"), ("x-c", "inbound")]);
        let mut rules = transform();
        rules.passthrough = PassthroughMode::All;
        rules.set.insert("x-a".to_owned(), "set".to_owned());
        rules.add.insert("x-d".to_owned(), "added".to_owned());
        rules.remove.push("x-c".to_owned());
        let endpoint = endpoint(Scheme::Http, "upstream", 8080);

        let outbound = transform_request_headers(&inbound, Some(&rules), &endpoint, None);

        assert_eq!(str_of(&outbound, "x-a"), Some("set"));
        assert_eq!(str_of(&outbound, "x-b"), Some("inbound"));
        assert!(outbound.get("x-c").is_none());
        assert_eq!(outbound.get_all("x-d").iter().count(), 1);
        assert_eq!(str_of(&outbound, "host"), Some("upstream:8080"));
    }

    #[test]
    fn a_configured_host_override_wins_over_the_endpoint_authority() {
        let inbound = HeaderMap::new();
        let mut rules = transform();
        rules
            .set
            .insert("Host".to_owned(), "internal.name".to_owned());
        let endpoint = endpoint(Scheme::Http, "upstream", 80);

        let outbound = transform_request_headers(&inbound, Some(&rules), &endpoint, None);

        assert_eq!(str_of(&outbound, "host"), Some("internal.name"));
    }

    #[test]
    fn host_authority_omits_the_standard_port() {
        assert_eq!(
            host_authority(&endpoint(Scheme::Http, "a.test", 80)),
            "a.test"
        );
        assert_eq!(
            host_authority(&endpoint(Scheme::Http, "a.test", 8080)),
            "a.test:8080"
        );
        assert_eq!(
            host_authority(&endpoint(Scheme::Https, "a.test", 443)),
            "a.test"
        );
        assert_eq!(
            host_authority(&endpoint(Scheme::Https, "a.test", 8443)),
            "a.test:8443"
        );
        assert_eq!(
            host_authority(&endpoint(Scheme::Wss, "a.test", 443)),
            "a.test"
        );
        assert_eq!(
            host_authority(&endpoint(Scheme::Grpc, "a.test", 443)),
            "a.test"
        );
    }

    #[test]
    fn host_authority_brackets_ipv6_literals() {
        // The stored form of an IPv6 endpoint is unbracketed (`validate_host`
        // strips the brackets); the authority puts them back, otherwise the
        // multi-colon string is not a parseable authority at all.
        assert_eq!(
            host_authority(&endpoint(Scheme::Http, "::1", 8080)),
            "[::1]:8080"
        );
        assert_eq!(
            host_authority(&endpoint(Scheme::Http, "2001:db8::1", 80)),
            "[2001:db8::1]"
        );
        assert_eq!(
            host_authority(&endpoint(Scheme::Https, "::1", 443)),
            "[::1]"
        );
    }

    #[test]
    fn host_authority_is_parseable_as_a_uri_authority() {
        for (host, port) in [
            ("127.0.0.1", 8080_u16),
            ("::1", 8080),
            ("2001:db8::1", 8080),
        ] {
            let authority = host_authority(&endpoint(Scheme::Http, host, port));
            let uri = http::Uri::builder()
                .scheme("http")
                .authority(authority.as_str())
                .path_and_query("/")
                .build()
                .unwrap_or_else(|error| panic!("'{authority}' is not an authority: {error}"));
            // `http` keeps the brackets of an IPv6 literal in the host.
            let expected_host = if host.contains(':') {
                format!("[{host}]")
            } else {
                host.to_owned()
            };
            assert_eq!(
                uri.host(),
                Some(expected_host.as_str()),
                "'{authority}' keeps the host"
            );
            assert_eq!(uri.port_u16(), Some(port), "'{authority}' keeps the port");
        }
    }

    #[test]
    fn response_headers_strip_hop_by_hop_then_apply_the_rules() {
        let mut headers = map(&[
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("server", "upstream"),
            ("x-drop", "yes"),
        ]);
        let mut rules = HeaderTransformResponse {
            set: BTreeMap::new(),
            add: BTreeMap::new(),
            remove: vec!["x-drop".to_owned()],
        };
        rules.set.insert("server".to_owned(), "oagw".to_owned());
        rules.add.insert("x-gateway".to_owned(), "oagw".to_owned());

        transform_response_headers(&mut headers, Some(&rules));

        assert_eq!(names(&headers), vec!["server", "x-gateway"]);
        assert_eq!(str_of(&headers, "server"), Some("oagw"));
    }

    #[test]
    fn response_headers_without_rules_only_strip_hop_by_hop() {
        let mut headers = map(&[
            ("transfer-encoding", "chunked"),
            ("content-type", "application/json"),
        ]);

        transform_response_headers(&mut headers, None);

        assert_eq!(names(&headers), vec!["content-type"]);
    }

    #[test]
    fn gateway_error_headers_carry_the_source_marker() {
        let headers = gateway_error_headers();

        assert_eq!(str_of(&headers, "x-oagw-error-source"), Some("gateway"));
        assert_eq!(
            str_of(&headers, "content-type"),
            Some("application/problem+json")
        );
    }

    #[test]
    fn non_http_schemes_are_reported_as_a_downstream_error() {
        let error = unsupported_scheme_error(Scheme::Https);
        assert_eq!(error.status().as_u16(), 502);
        assert!(error.detail().contains("https"));

        let error = unsupported_scheme_error(Scheme::Grpc);
        assert_eq!(
            error
                .extensions()
                .get("scheme")
                .and_then(|value| value.as_str()),
            Some("grpc")
        );
    }

    #[test]
    fn a_blocked_plaintext_scheme_names_the_configuration_that_blocked_it() {
        let error = http_scheme_not_allowed_error();

        assert_eq!(error.status().as_u16(), 502);
        assert_eq!(error.kind(), crate::error::OagwErrorKind::DownstreamError);
        assert_eq!(
            error
                .extensions()
                .get("scheme")
                .and_then(|value| value.as_str()),
            Some("http")
        );
        assert_eq!(
            error
                .extensions()
                .get("allow_http_upstream")
                .and_then(|value| value.as_bool()),
            Some(false),
            "the configuration that blocked the dial is named"
        );
        assert!(error.detail().contains("allow_http_upstream: false"));
    }

    // -- WebSocket upgrade detection (DESIGN §3.5, RFC 6455 §4.1) ----------

    fn handshake_headers(connection: &str, upgrade: &str) -> HeaderMap {
        map(&[
            ("connection", connection),
            ("upgrade", upgrade),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
        ])
    }

    #[test]
    fn a_get_with_upgrade_tokens_is_a_websocket_handshake() {
        let headers = handshake_headers("Upgrade", "websocket");
        assert!(is_websocket_upgrade(&Method::GET, &headers));

        // The token list is a list: `keep-alive` next to `upgrade` is still an
        // upgrade, whatever its case.
        let headers = handshake_headers("keep-alive, Upgrade", "WebSocket");
        assert!(is_websocket_upgrade(&Method::GET, &headers));
    }

    #[test]
    fn a_request_without_the_full_handshake_is_not_an_upgrade() {
        // Missing the `Connection` token.
        let headers = handshake_headers("keep-alive", "websocket");
        assert!(!is_websocket_upgrade(&Method::GET, &headers));

        // Missing (or mistyping) the `Upgrade` protocol: `websocket` is a token
        // matched as a whole, not a substring.
        for upgrade in ["", "h2c", "websocket-deflate", "WebSocket2"] {
            let headers = handshake_headers("Upgrade", upgrade);
            assert!(
                !is_websocket_upgrade(&Method::GET, &headers),
                "'{upgrade}' is not the websocket protocol"
            );
        }

        // A substring of the token list does not make an upgrade either.
        let headers = handshake_headers("keep-upgrade", "websocket");
        assert!(!is_websocket_upgrade(&Method::GET, &headers));

        // No `Upgrade` header at all.
        assert!(!is_websocket_upgrade(
            &Method::GET,
            &map(&[("connection", "upgrade")])
        ));

        // Only `GET` upgrades: a `CONNECT` here is an ordinary proxied request,
        // and an HTTP/2 extended `CONNECT` is out of scope for this slice.
        let headers = handshake_headers("Upgrade", "websocket");
        for method in [Method::POST, Method::CONNECT, Method::OPTIONS, Method::HEAD] {
            assert!(!is_websocket_upgrade(&method, &headers), "{method}");
        }
    }

    #[test]
    fn a_second_upgrade_header_after_a_first_one_is_still_an_upgrade() {
        // RFC 9110 §7.9 lets a sender repeat the `Upgrade` field, and hyper's
        // header map keeps both values. Reading only the first would hide the
        // handshake behind a first field that names something else — while
        // `connection_named` scans *all* of its own values, so the rule would be
        // half-blind about the two headers it pairs.
        let mut headers = handshake_headers("Upgrade", "h2c");
        headers.append(
            HeaderName::from_lowercase(b"upgrade").expect("a valid name"),
            HeaderValue::from_static("websocket"),
        );

        assert!(
            is_websocket_upgrade(&Method::GET, &headers),
            "the second value is the protocol the caller is offering"
        );

        // And the reverse order is the same answer, so the rule does not depend
        // on which value arrived first.
        let mut headers = handshake_headers("Upgrade", "websocket");
        headers.append(
            HeaderName::from_lowercase(b"upgrade").expect("a valid name"),
            HeaderValue::from_static("h2c"),
        );
        assert!(is_websocket_upgrade(&Method::GET, &headers));
    }

    #[test]
    fn the_handshake_headers_are_neither_gateway_owned_nor_credentials() {
        // They are the caller's contribution to a handshake the gateway only
        // signs, so the passthrough policy must be able to leave them alone --
        // and they are not credential material (RFC 6455 §1.3: the key exists to
        // defeat a naive intermediary, not to authenticate anyone).
        for name in [
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
        ] {
            let name = HeaderName::from_lowercase(name.as_bytes()).expect("a valid name");
            assert!(
                !is_gateway_owned(&name),
                "{name} is not derived by the gateway"
            );
            assert!(!is_credential(&name), "{name} is not a credential");
            assert!(!is_hop_by_hop(&name), "{name} is not connection-scoped");
        }
    }

    #[test]
    fn the_handshake_is_restored_verbatim_after_the_passthrough_policy() {
        // Under the default `passthrough: none` *nothing* inbound is forwarded,
        // so the handshake headers would otherwise be lost on the way out.
        let inbound = map(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "WebSocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "chat, superchat"),
            ("sec-websocket-extensions", "permessage-deflate"),
            ("authorization", "Bearer secret"),
            ("x-caller-header", "mine"),
        ]);
        let endpoint = endpoint(Scheme::Http, "10.0.0.1", 8080);
        let mut outbound = transform_request_headers(&inbound, None, &endpoint, None);

        restore_websocket_handshake(&mut outbound, &inbound);

        assert_eq!(
            str_of(&outbound, "sec-websocket-key"),
            Some("dGhlIHNhbXBsZSBub25jZQ=="),
            "the key travels unchanged: the gateway signs it, not rewrites it"
        );
        assert_eq!(str_of(&outbound, "sec-websocket-version"), Some("13"));
        assert_eq!(
            str_of(&outbound, "sec-websocket-protocol"),
            Some("chat, superchat")
        );
        assert_eq!(str_of(&outbound, "connection"), Some("Upgrade"));
        assert_eq!(str_of(&outbound, "upgrade"), Some("websocket"));
        assert!(
            outbound.get(WEBSOCKET_EXTENSIONS_HEADER).is_none(),
            "the gateway negotiates no extension, so none is offered"
        );
        assert!(
            outbound.get("authorization").is_none(),
            "the handshake does not become a credential bypass"
        );
        assert!(
            outbound.get("content-length").is_none() && outbound.get("transfer-encoding").is_none(),
            "a handshake carries no body"
        );
    }

    #[test]
    fn the_upstream_extension_offer_is_never_echoed_back() {
        let mut headers = map(&[
            ("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            ("sec-websocket-extensions", "permessage-deflate"),
            ("content-length", "0"),
            ("transfer-encoding", "chunked"),
            ("server", "upstream"),
        ]);

        strip_switched_protocol_framing(&mut headers);

        assert_eq!(names(&headers), vec!["sec-websocket-accept", "server"]);
    }
}
