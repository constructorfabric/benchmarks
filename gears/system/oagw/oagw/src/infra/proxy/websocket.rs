//! WebSocket proxying.
//!
//! `cpt-cf-oagw-fr-streaming` requires WebSocket session flows, not just
//! request/response. OAGW proxies them transparently: the client's
//! `Sec-WebSocket-Key` (and any subprotocol / extension offer) is forwarded
//! verbatim, so the upstream's `Sec-WebSocket-Accept` is already the correct
//! answer for the client and no re-keying is needed. Once both sides have
//! agreed, the two byte streams are spliced and OAGW stops interpreting the
//! traffic — frames, pings, closes and binary payloads all pass through
//! untouched.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use pingora_core::connectors::TransportConnector;
use pingora_core::protocols::Stream;
use pingora_core::upstreams::peer::HttpPeer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::OagwConfig;
use crate::domain::error::{
    ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, ErrorKind, OagwError, OagwResult,
};

/// Largest handshake response header block OAGW will buffer.
const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;
/// Header names that belong to the upgrade mechanism itself and are rebuilt
/// rather than copied from the transformed header set.
const UPGRADE_HEADERS: &[&str] = &[
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
    "host",
];

/// Whether the inbound request is a WebSocket upgrade.
#[must_use]
pub fn is_websocket_upgrade(method: &Method, headers: &HeaderMap) -> bool {
    if *method != Method::GET {
        return false;
    }
    let has_upgrade_token = headers
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
    let wants_websocket = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("websocket"));
    has_upgrade_token && wants_websocket
}

/// Outbound WebSocket connector.
pub struct WebSocketForwarder {
    connector: TransportConnector,
    config: Arc<OagwConfig>,
}

impl std::fmt::Debug for WebSocketForwarder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebSocketForwarder")
    }
}

/// The upstream side of a completed (or refused) handshake.
pub struct HandshakeOutcome {
    /// Upstream status line.
    pub status: StatusCode,
    /// Upstream response headers.
    pub headers: HeaderMap,
    /// Bytes already read past the end of the header block.
    pub leftover: Vec<u8>,
    /// The still-open transport.
    pub stream: Stream,
}

impl WebSocketForwarder {
    /// Build a forwarder.
    #[must_use]
    pub fn new(config: Arc<OagwConfig>) -> Self {
        Self {
            connector: TransportConnector::new(None),
            config,
        }
    }

    /// Open a transport to `peer` and perform the upgrade handshake.
    ///
    /// # Errors
    ///
    /// * `502 DownstreamError` — the transport could not be established.
    /// * `502 ProtocolError` — the upstream's handshake response is
    ///   unparseable or truncated.
    /// * `504 ConnectionTimeout` — the handshake exceeded the deadline.
    pub async fn handshake(
        &self,
        peer: &HttpPeer,
        authority: &str,
        path_and_query: &str,
        client_headers: &HeaderMap,
        forwarded: &HeaderMap,
    ) -> OagwResult<HandshakeOutcome> {
        let deadline = self.config.proxy_timeout();
        tokio::time::timeout(
            deadline,
            self.handshake_inner(peer, authority, path_and_query, client_headers, forwarded),
        )
        .await
        .map_err(|_| {
            OagwError::new(
                ErrorKind::ConnectionTimeout,
                format!(
                    "upstream did not complete the WebSocket handshake within {}s",
                    deadline.as_secs()
                ),
            )
            .with_retry_after(1)
        })?
    }

    async fn handshake_inner(
        &self,
        peer: &HttpPeer,
        authority: &str,
        path_and_query: &str,
        client_headers: &HeaderMap,
        forwarded: &HeaderMap,
    ) -> OagwResult<HandshakeOutcome> {
        let mut stream = self.connector.new_stream(peer).await.map_err(|err| {
            OagwError::new(
                ErrorKind::DownstreamError,
                format!("could not open a WebSocket transport to the upstream: {err}"),
            )
        })?;

        let request = build_handshake_request(authority, path_and_query, client_headers, forwarded);
        stream.write_all(request.as_bytes()).await.map_err(|err| {
            OagwError::new(
                ErrorKind::DownstreamError,
                format!("could not send the WebSocket handshake: {err}"),
            )
        })?;
        stream.flush().await.map_err(|err| {
            OagwError::new(
                ErrorKind::DownstreamError,
                format!("could not flush the WebSocket handshake: {err}"),
            )
        })?;

        let mut buffer = Vec::with_capacity(1024);
        let mut chunk = [0_u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.map_err(|err| {
                OagwError::new(
                    ErrorKind::ProtocolError,
                    format!("could not read the WebSocket handshake response: {err}"),
                )
            })?;
            if read == 0 {
                return Err(OagwError::new(
                    ErrorKind::ProtocolError,
                    "upstream closed the connection during the WebSocket handshake",
                ));
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.len() > MAX_HANDSHAKE_BYTES {
                return Err(OagwError::new(
                    ErrorKind::ProtocolError,
                    "upstream WebSocket handshake response header block is too large",
                ));
            }
            if let Some(parsed) = parse_handshake_response(&buffer)? {
                return Ok(HandshakeOutcome {
                    status: parsed.status,
                    headers: parsed.headers,
                    leftover: buffer[parsed.consumed..].to_vec(),
                    stream,
                });
            }
        }
    }
}

/// A parsed handshake response.
#[derive(Debug)]
struct ParsedHandshake {
    status: StatusCode,
    headers: HeaderMap,
    consumed: usize,
}

/// Render the outbound handshake request.
///
/// The upgrade headers are rebuilt from the client's offer so the handshake is
/// well-formed; everything else comes from the already-transformed header set,
/// which means `headers.set`/`add` rules and the auth plugin's injection apply
/// to a WebSocket exactly as they do to a plain request.
fn build_handshake_request(
    authority: &str,
    path_and_query: &str,
    client_headers: &HeaderMap,
    forwarded: &HeaderMap,
) -> String {
    let mut request = format!("GET {path_and_query} HTTP/1.1\r\nHost: {authority}\r\n");
    request.push_str("Connection: Upgrade\r\nUpgrade: websocket\r\n");
    for name in [
        header::SEC_WEBSOCKET_KEY,
        header::SEC_WEBSOCKET_VERSION,
        header::SEC_WEBSOCKET_PROTOCOL,
        header::SEC_WEBSOCKET_EXTENSIONS,
    ] {
        for value in client_headers.get_all(&name) {
            if let Ok(value) = value.to_str() {
                request.push_str(&format!("{name}: {value}\r\n"));
            }
        }
    }
    if client_headers.get(header::SEC_WEBSOCKET_VERSION).is_none() {
        request.push_str("Sec-WebSocket-Version: 13\r\n");
    }
    for (name, value) in forwarded {
        let lower = name.as_str().to_ascii_lowercase();
        if UPGRADE_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    request.push_str("\r\n");
    request
}

/// Try to parse a complete response header block out of `buffer`.
///
/// Returns `Ok(None)` when more bytes are needed.
fn parse_handshake_response(buffer: &[u8]) -> OagwResult<Option<ParsedHandshake>> {
    let mut header_slots = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut header_slots);
    let parsed = response.parse(buffer).map_err(|err| {
        OagwError::new(
            ErrorKind::ProtocolError,
            format!("malformed WebSocket handshake response: {err}"),
        )
    })?;
    let httparse::Status::Complete(consumed) = parsed else {
        return Ok(None);
    };
    let code = response.code.ok_or_else(|| {
        OagwError::new(
            ErrorKind::ProtocolError,
            "WebSocket handshake response has no status code",
        )
    })?;
    let status = StatusCode::from_u16(code).map_err(|_| {
        OagwError::new(
            ErrorKind::ProtocolError,
            format!("WebSocket handshake response has an invalid status code: {code}"),
        )
    })?;
    let mut headers = HeaderMap::new();
    for header in response.headers.iter() {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(header.name.to_ascii_lowercase()),
            HeaderValue::from_bytes(header.value),
        ) {
            headers.append(name, value);
        }
    }
    Ok(Some(ParsedHandshake {
        status,
        headers,
        consumed,
    }))
}

/// Build the `101 Switching Protocols` response relayed to the client.
#[must_use]
pub fn switching_protocols_response(upstream_headers: &HeaderMap) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    let out = response.headers_mut();
    out.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    out.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    for name in [
        header::SEC_WEBSOCKET_ACCEPT,
        header::SEC_WEBSOCKET_PROTOCOL,
        header::SEC_WEBSOCKET_EXTENSIONS,
    ] {
        for value in upstream_headers.get_all(&name) {
            out.append(name.clone(), value.clone());
        }
    }
    out.insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
    );
    response
}

/// Relay a handshake the upstream refused, so the client sees the upstream's
/// own answer rather than a synthetic gateway error.
#[must_use]
pub fn refused_handshake_response(outcome: &HandshakeOutcome) -> Response {
    let mut response = Response::new(Body::from(outcome.leftover.clone()));
    *response.status_mut() = outcome.status;
    let out = response.headers_mut();
    for (name, value) in &outcome.headers {
        if crate::infra::proxy::headers::is_stripped_inbound(name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out.insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
    );
    response
}

/// Splice the two byte streams until either side closes.
///
/// Any bytes the upstream already sent past the handshake boundary are
/// flushed to the client first, so a server that pushes a frame immediately
/// after the `101` does not lose it.
pub async fn splice(
    client: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    mut upstream: Stream,
    leftover: Vec<u8>,
) {
    let mut client = client;
    if !leftover.is_empty()
        && let Err(err) = client.write_all(&leftover).await
    {
        tracing::debug!(
            target: "oagw.websocket",
            error = %err,
            "could not flush buffered upstream bytes to the client"
        );
        return;
    }
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((to_upstream, to_client)) => tracing::debug!(
            target: "oagw.websocket",
            to_upstream,
            to_client,
            "websocket session closed"
        ),
        Err(err) => tracing::debug!(
            target: "oagw.websocket",
            error = %err,
            "websocket session ended with an error"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::try_from(*name).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn detects_a_websocket_upgrade() {
        let ws = headers(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
        assert!(is_websocket_upgrade(&Method::GET, &ws));
        // Browsers send `Connection: keep-alive, Upgrade`.
        let multi = headers(&[
            ("connection", "keep-alive, Upgrade"),
            ("upgrade", "WebSocket"),
        ]);
        assert!(is_websocket_upgrade(&Method::GET, &multi));
    }

    #[test]
    fn rejects_non_upgrades() {
        assert!(!is_websocket_upgrade(&Method::GET, &HeaderMap::new()));
        assert!(!is_websocket_upgrade(
            &Method::POST,
            &headers(&[("connection", "Upgrade"), ("upgrade", "websocket")])
        ));
        assert!(!is_websocket_upgrade(
            &Method::GET,
            &headers(&[("connection", "Upgrade"), ("upgrade", "h2c")])
        ));
        assert!(!is_websocket_upgrade(
            &Method::GET,
            &headers(&[("upgrade", "websocket")])
        ));
    }

    #[test]
    fn handshake_request_forwards_the_client_key_verbatim() {
        let client = headers(&[
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "chat"),
            ("connection", "Upgrade"),
            ("upgrade", "websocket"),
        ]);
        let forwarded = headers(&[
            ("authorization", "Bearer upstream-token"),
            ("x-custom", "v"),
        ]);
        let request =
            build_handshake_request("api.example.com", "/socket?x=1", &client, &forwarded);

        assert!(request.starts_with("GET /socket?x=1 HTTP/1.1\r\n"));
        assert!(request.contains("Host: api.example.com\r\n"));
        assert!(request.contains("Connection: Upgrade\r\n"));
        assert!(request.contains("Upgrade: websocket\r\n"));
        assert!(
            request.contains("sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n"),
            "the client's key is reused so its Accept stays valid: {request}"
        );
        assert!(request.contains("sec-websocket-protocol: chat\r\n"));
        assert!(request.contains("authorization: Bearer upstream-token\r\n"));
        assert!(request.contains("x-custom: v\r\n"));
        assert!(request.ends_with("\r\n\r\n"));
        // The forwarded set must not duplicate the upgrade mechanism.
        assert_eq!(request.matches("Upgrade: websocket").count(), 1);
    }

    #[test]
    fn handshake_request_defaults_the_version() {
        let client = headers(&[("sec-websocket-key", "abc")]);
        let request = build_handshake_request("h", "/", &client, &HeaderMap::new());
        assert!(request.contains("Sec-WebSocket-Version: 13\r\n"));
    }

    #[test]
    fn handshake_response_parsing_waits_for_the_full_block() {
        assert!(
            parse_handshake_response(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket")
                .unwrap()
                .is_none()
        );
        let complete = b"HTTP/1.1 101 Switching Protocols\r\n\
            Upgrade: websocket\r\n\
            Connection: Upgrade\r\n\
            Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
            \r\nFRAME";
        let parsed = parse_handshake_response(complete)
            .unwrap()
            .expect("complete");
        assert_eq!(parsed.status, StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(
            parsed.headers.get("sec-websocket-accept").unwrap(),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(&complete[parsed.consumed..], b"FRAME");
    }

    #[test]
    fn malformed_handshake_response_is_a_protocol_error() {
        let err = parse_handshake_response(b"NOT-HTTP\r\n\r\n").expect_err("malformed");
        assert_eq!(err.kind(), ErrorKind::ProtocolError);
    }

    #[test]
    fn switching_protocols_relays_the_negotiated_headers() {
        let upstream = headers(&[
            ("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
            ("sec-websocket-protocol", "chat"),
            ("x-ignored", "v"),
        ]);
        let response = switching_protocols_response(&upstream);
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        let out = response.headers();
        assert_eq!(out.get(header::CONNECTION).unwrap(), "upgrade");
        assert_eq!(out.get(header::UPGRADE).unwrap(), "websocket");
        assert_eq!(
            out.get(header::SEC_WEBSOCKET_ACCEPT).unwrap(),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(out.get(header::SEC_WEBSOCKET_PROTOCOL).unwrap(), "chat");
        assert!(out.get("x-ignored").is_none());
        assert_eq!(out.get(ERROR_SOURCE_HEADER).unwrap(), ERROR_SOURCE_UPSTREAM);
    }
}
