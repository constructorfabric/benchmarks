//! WebSocket upgrade proxying: upgrade recognition, upstream-first
//! negotiation, handshake header preservation, and bidirectional frame
//! relay (`cpt-cf-oagw-algo-upgrade-negotiation`,
//! `cpt-cf-oagw-algo-upgrade-header-preservation`, `cpt-cf-oagw-algo-frame-relay`,
//! `cpt-cf-oagw-dod-ws-upgrade-recognition`, `cpt-cf-oagw-dod-ws-upstream-first-upgrade`,
//! `cpt-cf-oagw-dod-ws-upgrade-headers-survive`, `cpt-cf-oagw-dod-ws-frame-relay`,
//! `cpt-cf-oagw-dod-ws-close-propagation`, `cpt-cf-oagw-dod-ws-upgrade-refusal`).
//!
//! [`negotiate_upstream`] uses `tokio_tungstenite::connect_async` toward the
//! selected endpoint and completes ONLY once the upstream itself answers
//! `101`; the caller-facing `axum::extract::ws::WebSocketUpgrade` is
//! completed by `crate::api::rest::handlers::proxy` only after this
//! function returns `Ok` (`cpt-cf-oagw-dod-ws-upstream-first-upgrade`), so a
//! refused upstream handshake never leaves an upgraded caller.
//!
//! `axum` builds its `WebSocket` type on top of the very same
//! `tokio-tungstenite`/`tungstenite` dependency this module uses directly
//! (one locked version across the workspace), so [`to_upstream_message`]
//! and [`to_client_message`] below are plain, lossless field-for-field
//! conversions between the two crates' otherwise-identical `Message` types.
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use axum::extract::ws::{CloseFrame as AxumCloseFrame, Message as AxumMessage, WebSocket};
use axum::http::header::{self, CONNECTION, UPGRADE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame as TungsteniteCloseFrame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::domain::model::Scheme;
use crate::domain::proxy::endpoint::TARGET_HOST_HEADER;
use crate::domain::proxy::guard::reject_dot_segments;
use crate::domain::proxy::headers::{
    apply_add, apply_remove, apply_set, forwarded_by_passthrough, hop_by_hop_names,
};
use crate::domain::resolve::RequestHeaderPlan;
use crate::error::OagwError;

/// The negotiated upstream WebSocket connection type: a plain or TLS
/// (`wss`) socket, matching `tokio-tungstenite`'s own `connect_async` return
/// type.
pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

// ---------------------------------------------------------------------------
// Upgrade recognition (`cpt-cf-oagw-algo-upgrade-negotiation`,
// `cpt-cf-oagw-dod-ws-upgrade-recognition`).
// ---------------------------------------------------------------------------

/// Recognises a proxy request as a WebSocket upgrade from its method and
/// upgrade-related headers alone, before any resolution-dependent state is
/// consulted, so the classification is independent of the resolved
/// endpoint (`cpt-cf-oagw-algo-upgrade-negotiation`,
/// `cpt-cf-oagw-dod-ws-upgrade-recognition`).
// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-recognition:p1:inst-ws-recognize-fn-01
#[must_use]
pub fn is_websocket_upgrade(method: &Method, headers: &HeaderMap) -> bool {
    *method == Method::GET
        && header_contains_token(headers, &UPGRADE, "websocket")
        && header_contains_token(headers, &CONNECTION, "upgrade")
        && headers.contains_key(&header::SEC_WEBSOCKET_KEY)
        && header_equals(headers, &header::SEC_WEBSOCKET_VERSION, "13")
}

/// `true` when `headers[name]` is a comma-separated list containing `token`,
/// compared case-insensitively (the `Connection`/`Upgrade` token grammar).
fn header_contains_token(headers: &HeaderMap, name: &HeaderName, token: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
}

/// `true` when `headers[name]` equals `expected`, compared case-insensitively.
fn header_equals(headers: &HeaderMap, name: &HeaderName, expected: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade-recognition:p1:inst-ws-recognize-fn-01

// ---------------------------------------------------------------------------
// Handshake header preservation
// (`cpt-cf-oagw-algo-upgrade-header-preservation`,
// `cpt-cf-oagw-dod-ws-upgrade-headers-survive`).
// ---------------------------------------------------------------------------

/// The four `Sec-WebSocket-*` handshake headers forwarded unchanged
/// regardless of the effective passthrough mode
/// (`cpt-cf-oagw-dod-ws-upgrade-headers-survive`).
fn always_forwarded_handshake_headers() -> [HeaderName; 4] {
    [
        HeaderName::from_static("sec-websocket-key"),
        HeaderName::from_static("sec-websocket-version"),
        HeaderName::from_static("sec-websocket-protocol"),
        HeaderName::from_static("sec-websocket-extensions"),
    ]
}

/// `true` when `name` must reach the upstream handshake: `Connection` and
/// `Upgrade` are exempted from the ordinary hop-by-hop strip
/// (`cpt-cf-oagw-dod-ws-upgrade-headers-survive`, step `inst-hdr-05`/`inst-hdr-06`),
/// the four `Sec-WebSocket-*` headers are always forwarded, `Host` and the
/// routing header stay consumed exactly as the plain path consumes them,
/// every other hop-by-hop header is still stripped, and anything left over
/// falls back to the ordinary passthrough rule.
fn is_forwarded_for_handshake(name: &HeaderName, plan: &RequestHeaderPlan) -> bool {
    if *name == CONNECTION || *name == UPGRADE {
        return true;
    }
    if *name == header::HOST || *name == TARGET_HOST_HEADER {
        return false;
    }
    if always_forwarded_handshake_headers().contains(name) {
        return true;
    }
    if hop_by_hop_names().contains(name) {
        return false;
    }
    forwarded_by_passthrough(name, plan)
}

/// Rebuilds the outbound WebSocket handshake headers: applies the ordinary
/// header-transformation plan (add/remove/set, then the `Host` rewrite)
/// while exempting `Connection`/`Upgrade` from hop-by-hop stripping and
/// forwarding the `Sec-WebSocket-*` headers unchanged
/// (`cpt-cf-oagw-algo-upgrade-header-preservation`,
/// `cpt-cf-oagw-dod-ws-upgrade-headers-survive`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a `set`/`add` header name or
/// value is malformed, or when `target_host` is not a valid header value.
// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-headers-survive:p1:inst-ws-handshake-headers-fn-01
pub fn build_handshake_headers(
    inbound: &HeaderMap,
    plan: &RequestHeaderPlan,
    target_host: &str,
) -> Result<HeaderMap, OagwError> {
    let mut outbound = HeaderMap::new();
    for (name, value) in inbound {
        if is_forwarded_for_handshake(name, plan) {
            outbound.append(name.clone(), value.clone());
        }
    }
    apply_remove(&mut outbound, &plan.remove);
    apply_set(&mut outbound, &plan.set)?;
    apply_add(&mut outbound, &plan.add)?;
    let host_value = HeaderValue::from_str(target_host).map_err(|_| {
        OagwError::validation_error("selected upstream host is not a valid header value")
    })?;
    outbound.insert(header::HOST, host_value);
    Ok(outbound)
}
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade-headers-survive:p1:inst-ws-handshake-headers-fn-01

// ---------------------------------------------------------------------------
// Upstream-first negotiation (`cpt-cf-oagw-algo-upgrade-negotiation`,
// `cpt-cf-oagw-dod-ws-upstream-first-upgrade`, `cpt-cf-oagw-dod-ws-upgrade-refusal`).
// ---------------------------------------------------------------------------

/// Builds the outbound target URL for a WebSocket upgrade.
///
/// # Errors
///
/// Returns [`OagwError::protocol_error`] (`502`) when `scheme` cannot serve
/// a WebSocket upgrade — every scheme other than `ws`/`wss`, including the
/// declared-but-unimplemented `wt` (`cpt-cf-oagw-dod-ws-upgrade-refusal`);
/// returns [`OagwError::validation_error`] (`400`) when `path_and_query`'s
/// path portion carries a `.` or `..` segment (BUG2-F-001), the same
/// exposure `crate::domain::proxy::guard::enforce_path_suffix` closes for
/// the plain-HTTP target — checked again here, independently, at the point
/// this target string is actually built.
// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-refusal:p1:inst-ws-target-url-fn-01
pub fn build_ws_target_url(
    scheme: Scheme,
    host: &str,
    port: u16,
    path_and_query: &str,
) -> Result<url::Url, OagwError> {
    let scheme_str = match scheme {
        Scheme::Ws => "ws",
        Scheme::Wss => "wss",
        _ => {
            return Err(OagwError::protocol_error(format!(
                "endpoint scheme '{scheme:?}' cannot serve a WebSocket upgrade"
            )));
        }
    };
    let path_only = path_and_query.split('?').next().unwrap_or(path_and_query);
    reject_dot_segments(path_only)?;
    let raw = format!("{scheme_str}://{host}:{port}{path_and_query}");
    url::Url::parse(&raw)
        .map_err(|_| OagwError::validation_error(format!("'{raw}' is not a valid target URL")))
}
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade-refusal:p1:inst-ws-target-url-fn-01

/// Builds the outbound handshake request: `GET` to `url` carrying `headers`
/// unchanged (the client's original `Sec-WebSocket-Key` included), matching
/// `tungstenite::handshake::client::generate_request`'s requirement that
/// `Host`, `Connection`, `Upgrade`, `Sec-WebSocket-Version`, and
/// `Sec-WebSocket-Key` already be present.
fn build_handshake_request(
    url: &url::Url,
    headers: HeaderMap,
) -> Result<axum::http::Request<()>, OagwError> {
    let mut request = axum::http::Request::builder()
        .method(Method::GET)
        .uri(url.as_str())
        .body(())
        .map_err(|error| {
            OagwError::validation_error(format!(
                "failed to build the upstream WebSocket handshake request: {error}"
            ))
        })?;
    *request.headers_mut() = headers;
    Ok(request)
}

/// Sends the WebSocket handshake to the selected upstream endpoint and
/// completes only once the upstream answers `101` with a matching
/// `Sec-WebSocket-Accept` — verified by `tungstenite` itself before this
/// future resolves — bounding the handshake by `timeout`, never a
/// subsequently open session (`cpt-cf-oagw-algo-upgrade-negotiation`,
/// `cpt-cf-oagw-dod-ws-upstream-first-upgrade`, `cpt-cf-oagw-dod-longlived-pipeline`).
///
/// `build_handshake_headers` forwards the caller's `Sec-WebSocket-Extensions`
/// offer to the upstream unchanged (`cpt-cf-oagw-dod-ws-upgrade-headers-survive`),
/// but [`relay`] itself never implements any negotiated extension: it moves
/// frames between the two `tungstenite`-backed sockets exactly as read, with
/// no decode/encode step for a payload an extension (e.g. `permessage-deflate`)
/// would have transformed. An upstream that accepts such an offer is
/// therefore free, per the extension's own contract, to start sending frames
/// this relay cannot parse — `tungstenite`'s reader rejects the resulting
/// non-zero reserved bits as a protocol violation once such a frame arrives,
/// well after the caller-facing socket has already been completed to `101`,
/// tearing down a session that looked healthy at handshake time. Refusing
/// here, before the caller-facing upgrade is ever completed, keeps that
/// failure an ordinary upstream-refusal response instead of a session that
/// opens successfully and then silently breaks
/// (`cpt-cf-oagw-dod-ws-upgrade-refusal`).
///
/// # Errors
///
/// Returns [`OagwError::connection_timeout`] when the handshake does not
/// complete within `timeout`, and [`OagwError::protocol_error`] (`502`) for
/// any refusal: a non-`101` handshake response, a refused/failed transport
/// connection, an upstream that answered with a `Sec-WebSocket-Extensions`
/// this relay cannot honor, or any other negotiation failure
/// (`cpt-cf-oagw-dod-ws-upgrade-refusal`).
// @cpt-begin:cpt-cf-oagw-dod-ws-upstream-first-upgrade:p1:inst-ws-negotiate-fn-01
pub async fn negotiate_upstream(
    url: url::Url,
    headers: HeaderMap,
    timeout: std::time::Duration,
) -> Result<WsStream, OagwError> {
    let request = build_handshake_request(&url, headers)?;
    match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(request)).await {
        Err(_) => Err(OagwError::connection_timeout(
            "upstream WebSocket handshake did not complete within the configured proxy timeout",
        )),
        Ok(Err(error)) => Err(map_negotiation_error(&error)),
        Ok(Ok((stream, response))) => {
            if response
                .headers()
                .contains_key(&header::SEC_WEBSOCKET_EXTENSIONS)
            {
                return Err(OagwError::protocol_error(
                    "upstream accepted a WebSocket extension this gateway cannot relay",
                ));
            }
            Ok(stream)
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-ws-upstream-first-upgrade:p1:inst-ws-negotiate-fn-01

/// Maps a `tungstenite` handshake failure onto the gateway-sourced
/// protocol-error type: `Error::Http` is the documented non-`101` response
/// case, every other variant (refused/reset transport, invalid URL, etc.)
/// falls into the same bucket since no upstream WebSocket session was ever
/// established (`cpt-cf-oagw-dod-ws-upgrade-refusal`).
// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-refusal:p1:inst-ws-negotiate-error-fn-01
fn map_negotiation_error(error: &tokio_tungstenite::tungstenite::Error) -> OagwError {
    use tokio_tungstenite::tungstenite::Error;
    match error {
        Error::Http(response) => OagwError::protocol_error(format!(
            "upstream answered the WebSocket handshake with status {} instead of 101",
            response.status()
        )),
        other => OagwError::protocol_error(format!("upstream WebSocket handshake failed: {other}")),
    }
}
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade-refusal:p1:inst-ws-negotiate-error-fn-01

// ---------------------------------------------------------------------------
// Bidirectional frame relay (`cpt-cf-oagw-algo-frame-relay`,
// `cpt-cf-oagw-dod-ws-frame-relay`, `cpt-cf-oagw-dod-ws-close-propagation`).
// ---------------------------------------------------------------------------

/// Relays WebSocket frames bidirectionally between the caller-facing socket
/// and the negotiated upstream socket, preserving frame kind, payload
/// bytes, and per-direction ordering with no rewriting, until either
/// direction sees a close frame or either half drops, then tears down both
/// halves (`cpt-cf-oagw-algo-frame-relay`, `cpt-cf-oagw-dod-ws-frame-relay`,
/// `cpt-cf-oagw-dod-ws-close-propagation`, `cpt-cf-oagw-algo-session-teardown`).
// @cpt-begin:cpt-cf-oagw-dod-ws-frame-relay:p1:inst-ws-relay-fn-01
// @cpt-begin:cpt-cf-oagw-dod-ws-close-propagation:p1:inst-ws-relay-fn-01
pub async fn relay(client: WebSocket, upstream: WsStream) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    tokio::select! {
        () = relay_client_to_upstream(&mut client_rx, &mut upstream_tx) => {}
        () = relay_upstream_to_client(&mut upstream_rx, &mut client_tx) => {}
    }

    // Session teardown: whichever side ended first, release both halves
    // promptly rather than draining the other (`cpt-cf-oagw-algo-session-teardown`).
    // Both sockets are being discarded regardless of outcome, so a close
    // error carries nothing actionable; `drop` makes that discard explicit.
    drop(upstream_tx.close().await);
    drop(client_tx.close().await);
}

/// The client-to-upstream relay direction: reads client frames and writes
/// them to the upstream socket until a close frame is forwarded or either
/// side errors/drops.
async fn relay_client_to_upstream(
    client_rx: &mut SplitStream<WebSocket>,
    upstream_tx: &mut SplitSink<WsStream, TungsteniteMessage>,
) {
    while let Some(Ok(message)) = client_rx.next().await {
        let converted = to_upstream_message(message);
        let is_close = matches!(converted, TungsteniteMessage::Close(_));
        if upstream_tx.send(converted).await.is_err() || is_close {
            break;
        }
    }
}

/// The upstream-to-client relay direction: reads upstream frames and writes
/// them to the caller-facing socket until a close frame is forwarded or
/// either side errors/drops.
async fn relay_upstream_to_client(
    upstream_rx: &mut SplitStream<WsStream>,
    client_tx: &mut SplitSink<WebSocket, AxumMessage>,
) {
    while let Some(Ok(message)) = upstream_rx.next().await {
        let Some(converted) = to_client_message(message) else {
            continue;
        };
        let is_close = matches!(converted, AxumMessage::Close(_));
        if client_tx.send(converted).await.is_err() || is_close {
            break;
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-ws-close-propagation:p1:inst-ws-relay-fn-01
// @cpt-end:cpt-cf-oagw-dod-ws-frame-relay:p1:inst-ws-relay-fn-01

/// Converts a caller-facing frame into its upstream-bound equivalent,
/// preserving frame kind and payload bytes exactly
/// (`cpt-cf-oagw-dod-ws-frame-relay`).
fn to_upstream_message(message: AxumMessage) -> TungsteniteMessage {
    match message {
        AxumMessage::Text(text) => TungsteniteMessage::Text(text.as_str().into()),
        AxumMessage::Binary(data) => TungsteniteMessage::Binary(data),
        AxumMessage::Ping(data) => TungsteniteMessage::Ping(data),
        AxumMessage::Pong(data) => TungsteniteMessage::Pong(data),
        AxumMessage::Close(Some(frame)) => TungsteniteMessage::Close(Some(TungsteniteCloseFrame {
            code: frame.code.into(),
            reason: frame.reason.as_str().into(),
        })),
        AxumMessage::Close(None) => TungsteniteMessage::Close(None),
    }
}

/// Converts an upstream frame into its caller-bound equivalent, preserving
/// frame kind and payload bytes exactly (`cpt-cf-oagw-dod-ws-frame-relay`).
/// Returns `None` for `tungstenite`'s raw `Frame` variant, which its own
/// documentation says a reader never actually observes.
fn to_client_message(message: TungsteniteMessage) -> Option<AxumMessage> {
    Some(match message {
        TungsteniteMessage::Text(text) => AxumMessage::Text(text.as_str().into()),
        TungsteniteMessage::Binary(data) => AxumMessage::Binary(data),
        TungsteniteMessage::Ping(data) => AxumMessage::Ping(data),
        TungsteniteMessage::Pong(data) => AxumMessage::Pong(data),
        TungsteniteMessage::Close(Some(frame)) => AxumMessage::Close(Some(AxumCloseFrame {
            code: frame.code.into(),
            reason: frame.reason.as_str().into(),
        })),
        TungsteniteMessage::Close(None) => AxumMessage::Close(None),
        TungsteniteMessage::Frame(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        build_handshake_headers, build_ws_target_url, is_websocket_upgrade, negotiate_upstream,
    };
    use crate::domain::model::Scheme;
    use crate::domain::resolve::RequestHeaderPlan;
    use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn upgrade_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        headers.insert("connection", HeaderValue::from_static("Upgrade"));
        headers.insert(
            "sec-websocket-key",
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
        headers
    }

    // @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-recognition:p1:inst-ws-recognize-positive-test-01
    #[test]
    fn a_well_formed_upgrade_request_is_recognised() {
        assert!(is_websocket_upgrade(&Method::GET, &upgrade_headers()));
    }
    // @cpt-end:cpt-cf-oagw-dod-ws-upgrade-recognition:p1:inst-ws-recognize-positive-test-01

    #[test]
    fn a_post_method_is_never_recognised_as_an_upgrade() {
        assert!(!is_websocket_upgrade(&Method::POST, &upgrade_headers()));
    }

    #[test]
    fn a_missing_sec_websocket_key_is_not_recognised() {
        let mut headers = upgrade_headers();
        headers.remove("sec-websocket-key");
        assert!(!is_websocket_upgrade(&Method::GET, &headers));
    }

    #[test]
    fn a_wrong_websocket_version_is_not_recognised() {
        let mut headers = upgrade_headers();
        headers.insert("sec-websocket-version", HeaderValue::from_static("8"));
        assert!(!is_websocket_upgrade(&Method::GET, &headers));
    }

    #[test]
    fn a_connection_header_without_the_upgrade_token_is_not_recognised() {
        let mut headers = upgrade_headers();
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        assert!(!is_websocket_upgrade(&Method::GET, &headers));
    }

    #[test]
    fn an_ordinary_get_request_is_not_recognised() {
        assert!(!is_websocket_upgrade(&Method::GET, &HeaderMap::new()));
    }

    // @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-headers-survive:p1:inst-ws-handshake-headers-test-01
    #[test]
    fn connection_and_upgrade_survive_while_ordinary_hop_by_hop_headers_are_stripped() {
        let mut inbound = upgrade_headers();
        inbound.insert("te", HeaderValue::from_static("trailers"));
        inbound.insert("keep-alive", HeaderValue::from_static("timeout=5"));

        let plan = RequestHeaderPlan::default();
        let outbound =
            build_handshake_headers(&inbound, &plan, "upstream.example.com").expect("must build");

        assert_eq!(
            outbound.get("connection").and_then(|v| v.to_str().ok()),
            Some("Upgrade")
        );
        assert_eq!(
            outbound.get("upgrade").and_then(|v| v.to_str().ok()),
            Some("websocket")
        );
        assert!(!outbound.contains_key("te"));
        assert!(!outbound.contains_key("keep-alive"));
    }
    // @cpt-end:cpt-cf-oagw-dod-ws-upgrade-headers-survive:p1:inst-ws-handshake-headers-test-01

    // @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-headers-survive:p1:inst-ws-handshake-sec-headers-test-01
    #[test]
    fn sec_websocket_headers_survive_a_default_none_passthrough_mode() {
        let mut inbound = upgrade_headers();
        inbound.insert("sec-websocket-protocol", HeaderValue::from_static("chat"));
        inbound.insert(
            "sec-websocket-extensions",
            HeaderValue::from_static("permessage-deflate"),
        );

        let plan = RequestHeaderPlan::default();
        let outbound =
            build_handshake_headers(&inbound, &plan, "upstream.example.com").expect("must build");

        assert_eq!(
            outbound
                .get("sec-websocket-key")
                .and_then(|v| v.to_str().ok()),
            Some("dGhlIHNhbXBsZSBub25jZQ==")
        );
        assert_eq!(
            outbound
                .get("sec-websocket-version")
                .and_then(|v| v.to_str().ok()),
            Some("13")
        );
        assert_eq!(
            outbound
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok()),
            Some("chat")
        );
        assert_eq!(
            outbound
                .get("sec-websocket-extensions")
                .and_then(|v| v.to_str().ok()),
            Some("permessage-deflate")
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-ws-upgrade-headers-survive:p1:inst-ws-handshake-sec-headers-test-01

    #[test]
    fn host_is_rewritten_to_the_selected_endpoint() {
        let inbound = upgrade_headers();
        let plan = RequestHeaderPlan::default();
        let outbound =
            build_handshake_headers(&inbound, &plan, "upstream.example.com").expect("must build");
        assert_eq!(
            outbound.get("host").and_then(|v| v.to_str().ok()),
            Some("upstream.example.com")
        );
    }

    // BUG2-F-001 regression: `build_ws_target_url` closes the same
    // dot-segment escape independently of the path-suffix guard.

    #[test]
    fn a_dot_dot_segment_is_rejected_when_building_the_ws_target_url() {
        let error = build_ws_target_url(
            Scheme::Ws,
            "upstream.example.com",
            80,
            "/v1/items/../../secret",
        )
        .expect_err("a '..' segment must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_legitimate_dotted_filename_suffix_is_allowed_in_the_ws_target_url() {
        let url = build_ws_target_url(
            Scheme::Ws,
            "upstream.example.com",
            8080,
            "/v1/items/report.v2.json",
        )
        .expect("a dotted filename must not be treated as a dot-segment");
        assert_eq!(
            url.as_str(),
            "ws://upstream.example.com:8080/v1/items/report.v2.json"
        );
    }

    // Regression test for the extension-mismatch bug found while diagnosing
    // a WebSocket relay that completed the handshake (`101`) and relayed the
    // client's first frame, then silently died once the upstream's reply
    // used a `Sec-WebSocket-Extensions` the relay never implements: a real
    // upstream is free to start using an extension the instant it accepts
    // the offer `build_handshake_headers` forwards unchanged, and `relay`
    // has no decode step for it, so `negotiate_upstream` must refuse before
    // the caller-facing socket is ever completed to `101`, rather than let a
    // session open and then break the first time the extension is actually
    // used (`cpt-cf-oagw-dod-ws-upgrade-refusal`).
    #[tokio::test]
    async fn an_upstream_that_accepts_an_extension_this_relay_cannot_honor_is_refused() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind must succeed");
        let addr = listener.local_addr().expect("local_addr must succeed");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0_u8; 1024];
                drop(socket.read(&mut buf).await);
                // The well-known RFC 6455 `Sec-WebSocket-Key`/`-Accept` pair
                // matching `upgrade_headers()` below, plus an accepted
                // `Sec-WebSocket-Extensions` this relay cannot decode.
                let response = "HTTP/1.1 101 Switching Protocols\r\n\
                     Connection: Upgrade\r\n\
                     Upgrade: websocket\r\n\
                     Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
                     Sec-WebSocket-Extensions: permessage-deflate\r\n\r\n";
                drop(socket.write_all(response.as_bytes()).await);
                drop(socket.flush().await);
            }
        });

        let mut headers = upgrade_headers();
        headers.insert(
            "sec-websocket-extensions",
            HeaderValue::from_static("permessage-deflate"),
        );
        headers.insert(
            "host",
            HeaderValue::from_str(&format!("127.0.0.1:{}", addr.port())).expect("valid host"),
        );
        let url = build_ws_target_url(Scheme::Ws, "127.0.0.1", addr.port(), "/ws")
            .expect("target url must build");

        let error = negotiate_upstream(url, headers, Duration::from_secs(2))
            .await
            .expect_err("an upstream accepting an unsupported extension must be refused");
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);
        assert!(
            error.to_problem().detail.contains("extension"),
            "refusal must be attributed to the unsupported extension, got: {}",
            error.to_problem().detail
        );
    }
}
