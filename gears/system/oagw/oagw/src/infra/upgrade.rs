//! WebSocket / WebTransport upgrade tunneling of the proxy data plane
//! ([PRD.md](../../../docs/PRD.md) "Streaming support: WebSocket, WebTransport",
//! [DESIGN.md](../../../docs/DESIGN.md) "Proxy Request Flow").
//!
//! An upgrade request is recognised by two headers and only two: a `Connection`
//! whose token list names `upgrade`, and an `Upgrade` naming `websocket` or
//! `wt`. Every other request — including one that carries an `Upgrade` of
//! another protocol — stays on the plain HTTP path of
//! [`crate::infra::http_client`].
//!
//! After the `101` the tunnel is a byte splice, never a frame parser: the proxy
//! relays the upstream's handshake and then pumps both directions with
//! [`tokio::io::copy_bidirectional`] until either side closes. Frames are never
//! parsed or inspected, so the tunnel is agnostic of what flows through it: a
//! future TLS connector lights up `wss` and `wt` endpoints through this very
//! code, which today only dials plaintext `http` endpoints (see
//! [`crate::api::rest::proxy`]'s scheme gate).
//!
//! The handshake itself is forwarded and relayed untouched, whatever the
//! passthrough policy of the upstream says: `Connection`, `Upgrade` and every
//! `Sec-WebSocket-*` header are part of the protocol being switched to, and
//! rewriting them breaks the handshake rather than the policy.

use std::io;

use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;

/// The `Connection` token that marks a request as an upgrade.
const CONNECTION_UPGRADE_TOKEN: &str = "upgrade";

/// The `Upgrade` protocols this build tunnels. Anything else — `h2c`, `TLS/1.2`
/// — is a request class the plain HTTP path handles.
const TUNNELLED_PROTOCOLS: [&str; 2] = ["websocket", "wt"];

/// Prefix of the WebSocket handshake headers. They are part of the upgrade and
/// survive every passthrough policy: dropping `Sec-WebSocket-Key` would leave
/// the upstream nothing to accept.
const WEBSOCKET_HEADER_PREFIX: &str = "sec-websocket";

/// The body framing headers a `101` must not carry (RFC 9110 §15.2.1: the
/// protocol switches and no message follows).
const RESPONSE_FRAMING_HEADERS: [&str; 2] = ["content-length", "transfer-encoding"];

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// `true` when `headers` carry both an upgrade token in `Connection` and an
/// `Upgrade` of `websocket` or `wt`.
///
/// Both halves are required: a bare `Upgrade` header is a request class a plain
/// proxy ignores, and a `Connection: upgrade` without an `Upgrade` names
/// nothing to switch to.
#[must_use]
pub fn is_upgrade_request(headers: &http::HeaderMap) -> bool {
    carries_token(
        headers.get_all(http::header::CONNECTION).iter(),
        CONNECTION_UPGRADE_TOKEN,
    ) && headers
        .get_all(http::header::UPGRADE)
        .iter()
        .any(|value| tokens(value).any(is_tunnelled))
}

/// `true` when `protocol` is one of [`TUNNELLED_PROTOCOLS`].
fn is_tunnelled(protocol: &str) -> bool {
    TUNNELLED_PROTOCOLS
        .iter()
        .any(|tunnelled| protocol.eq_ignore_ascii_case(tunnelled))
}

/// The comma-separated tokens of one header value, as sent.
fn tokens(value: &http::HeaderValue) -> impl Iterator<Item = &str> {
    value.to_str().unwrap_or_default().split(',').map(str::trim)
}

/// `true` when one of `values`' comma-separated token lists names `token`.
fn carries_token<'a>(values: impl Iterator<Item = &'a http::HeaderValue>, token: &str) -> bool {
    values
        .flat_map(tokens)
        .any(|candidate| candidate.eq_ignore_ascii_case(token))
}

// ---------------------------------------------------------------------------
// Forwarded request
// ---------------------------------------------------------------------------

/// The handshake headers of an upgrade request, as they must reach the
/// upstream: verbatim, whatever the passthrough policy of the upstream is.
///
/// `Host` is left out: the endpoint supplies its own, and the caller's would
/// override it.
#[must_use]
pub fn handshake_headers(headers: &http::HeaderMap) -> http::HeaderMap {
    let mut handshake = http::HeaderMap::new();
    for (name, value) in headers {
        if is_handshake_header(name) && *name != http::header::HOST {
            handshake.append(name.clone(), value.clone());
        }
    }
    handshake
}

/// `true` when `name` is a header an upgrade request may not lose.
fn is_handshake_header(name: &http::HeaderName) -> bool {
    let name = name.as_str();
    name == "connection" || name == "upgrade" || name.starts_with(WEBSOCKET_HEADER_PREFIX)
}

/// The outbound headers of an upgrade request: the planned headers of the
/// normal path, with the caller's handshake restored over whatever the plan
/// dropped or a header rule replaced.
///
/// `Connection` and `Upgrade` are replaced rather than appended, so the
/// handshake the caller sent is the only value on the wire and the upstream
/// reads a `Connection` header it can act on.
#[must_use]
pub fn planned_request_headers(
    planned: &http::HeaderMap,
    handshake: &http::HeaderMap,
) -> http::HeaderMap {
    let mut headers = planned.clone();
    for (name, value) in handshake {
        if name == http::header::CONNECTION || name == http::header::UPGRADE {
            headers.remove(name);
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

// ---------------------------------------------------------------------------
// Relayed response
// ---------------------------------------------------------------------------

/// The headers the caller's `101` carries: the upstream's handshake, relayed
/// verbatim, minus the body framing headers a switched protocol has no use for.
///
/// Nothing here is stripped as hop-by-hop: `Connection` and `Upgrade` *are* the
/// handshake, and `Sec-WebSocket-Accept` is the proof the upstream answered it.
#[must_use]
pub fn relayed_response_headers(upstream: &http::HeaderMap) -> http::HeaderMap {
    let mut relayed = http::HeaderMap::with_capacity(upstream.len());
    for (name, value) in upstream {
        if RESPONSE_FRAMING_HEADERS.contains(&name.as_str()) {
            continue;
        }
        relayed.append(name.clone(), value.clone());
    }
    relayed
}

// ---------------------------------------------------------------------------
// Splice
// ---------------------------------------------------------------------------

/// Splices the two ends of an accepted upgrade for the lifetime of the session.
///
/// Both ends are the raw sockets the two HTTP conversations switched to: bytes
/// flow in both directions untouched until one of them closes, which ends the
/// session and returns the byte counts of the two directions.
///
/// Each end speaks hyper's own I/O traits (`hyper::upgrade::Upgraded`), so it is
/// wrapped in [`TokioIo`] to be pumped by [`tokio::io::copy_bidirectional`].
///
/// # Errors
/// When an end never delivered its socket, or the pipe broke mid-session. The
/// session is over either way; the caller only logs it.
pub async fn tunnel(client: OnUpgrade, upstream: OnUpgrade) -> io::Result<(u64, u64)> {
    let (mut client, mut upstream) = match (client.await, upstream.await) {
        (Ok(client), Ok(upstream)) => (TokioIo::new(client), TokioIo::new(upstream)),
        (Err(error), _) | (_, Err(error)) => {
            return Err(io::Error::other(format!("the upgrade failed: {error}")));
        }
    };
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use http::HeaderValue;

    use super::{
        handshake_headers, is_upgrade_request, planned_request_headers, relayed_response_headers,
    };

    fn headers<'a>(entries: &'a [(&'a str, &'a str)]) -> http::HeaderMap {
        entries
            .iter()
            .map(|(name, value)| {
                let name = name.to_ascii_lowercase();
                (
                    http::HeaderName::from_lowercase(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn an_upgrade_is_connection_and_upgrade_together() {
        let websocket = headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "WebSocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ]);
        assert!(is_upgrade_request(&websocket));

        let webtransport = headers(&[("Connection", "keep-alive, Upgrade"), ("Upgrade", "wt")]);
        assert!(is_upgrade_request(&webtransport));

        // A `Connection` that does not name `upgrade` is a plain request,
        // however much the caller would like to switch protocols.
        let no_connection = headers(&[("connection", "keep-alive"), ("upgrade", "websocket")]);
        assert!(!is_upgrade_request(&no_connection));

        // And so is an upgrade this build does not tunnel.
        let other_protocol = headers(&[("Connection", "upgrade"), ("Upgrade", "h2c")]);
        assert!(!is_upgrade_request(&other_protocol));

        assert!(!is_upgrade_request(&http::HeaderMap::new()));
    }

    #[test]
    fn the_handshake_survives_any_passthrough_policy() {
        let inbound = headers(&[
            ("host", "oagw.example.com"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "chat, superchat"),
            ("sec-websocket-extensions", "permessage-deflate"),
            ("authorization", "Bearer token"),
        ]);
        let handshake = handshake_headers(&inbound);
        for name in [
            "connection",
            "upgrade",
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
            "sec-websocket-extensions",
        ] {
            assert_eq!(handshake[name].to_str().unwrap(), inbound[name], "{name}");
        }
        // The caller's host is replaced by the endpoint's, so it stays out.
        assert!(handshake.get("host").is_none());
        assert!(handshake.get("authorization").is_none());
    }

    #[test]
    fn the_planned_headers_keep_the_handshake_over_the_plan() {
        let planned = headers(&[
            ("host", "us.vendor.com"),
            ("connection", "keep-alive"),
            ("upgrade", "SPDY/3.1"),
            ("x-kept", "yes"),
        ]);
        let handshake = handshake_headers(&headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-key", "key"),
        ]));
        let outbound = planned_request_headers(&planned, &handshake);
        assert_eq!(outbound["connection"], "Upgrade");
        assert_eq!(outbound["upgrade"], "websocket");
        assert_eq!(outbound["sec-websocket-key"], "key");
        assert_eq!(outbound["host"], "us.vendor.com");
        assert_eq!(outbound["x-kept"], "yes");
    }

    #[test]
    fn the_101_is_relayed_with_its_handshake() {
        let upstream = headers(&[
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
            ("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            ("sec-websocket-protocol", "chat"),
            ("sec-websocket-extensions", "permessage-deflate"),
        ]);
        let relayed = relayed_response_headers(&upstream);
        assert_eq!(relayed.len(), upstream.len());
        for (name, value) in &upstream {
            assert_eq!(relayed[name], *value, "{name}");
        }

        // A 101 has no body, so the framing headers of the upstream are not
        // relayed even if it sent one.
        let upstream = headers(&[
            ("upgrade", "websocket"),
            ("content-length", "0"),
            ("transfer-encoding", "chunked"),
        ]);
        let relayed = relayed_response_headers(&upstream);
        assert_eq!(relayed.len(), 1);
        assert!(relayed.get("upgrade").is_some());
    }
}
