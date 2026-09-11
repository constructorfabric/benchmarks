//! Protocol-upgrade proxying (WebSocket and any other `Upgrade:` handshake).
//!
//! An upgraded connection stops being HTTP after the `101`, so the exchange is
//! driven at the byte level: the request line and headers are written by hand,
//! the response head is parsed with `httparse`, and everything after it is
//! spliced verbatim in both directions. That keeps subprotocol negotiation,
//! extensions and framing entirely between the two peers.

use bytes::{Bytes, BytesMut};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use pingora_core::protocols::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::domain::error::{ErrorKind, OagwError, OagwResult};

/// Largest response head accepted from an upstream during an upgrade.
const MAX_HEAD_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 128;

/// The upstream's answer to an upgrade request.
pub struct UpgradeExchange {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// Bytes already read past the response head — must be replayed to the
    /// client before the two streams are spliced.
    pub leftover: Bytes,
    pub stream: Stream,
}

/// Whether a request asks for a protocol upgrade.
#[must_use]
pub fn is_upgrade_request(headers: &HeaderMap) -> bool {
    let connection_upgrade = headers
        .get_all(http::header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| {
            v.split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
    connection_upgrade && headers.contains_key(http::header::UPGRADE)
}

/// Serialize the request head for an upgrade handshake.
#[must_use]
pub fn encode_request_head(
    method: &str,
    path_and_query: &str,
    host: &str,
    headers: &HeaderMap,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(path_and_query.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    out.extend_from_slice(b"Host: ");
    out.extend_from_slice(host.as_bytes());
    out.extend_from_slice(b"\r\n");
    for (name, value) in headers {
        if name.as_str().eq_ignore_ascii_case("host") {
            continue;
        }
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// Send the handshake and read the upstream's response head.
///
/// # Errors
/// `502 ProtocolError` on a malformed or oversized response head, `502
/// StreamAborted` when the upstream closes before answering.
pub async fn perform_handshake(
    mut stream: Stream,
    method: &str,
    path_and_query: &str,
    host: &str,
    headers: &HeaderMap,
) -> OagwResult<UpgradeExchange> {
    let head = encode_request_head(method, path_and_query, host, headers);
    stream.write_all(&head).await.map_err(|err| {
        OagwError::new(
            ErrorKind::ProtocolError,
            format!("failed to write upgrade request to upstream: {err}"),
        )
    })?;
    stream.flush().await.map_err(|err| {
        OagwError::new(
            ErrorKind::ProtocolError,
            format!("failed to flush upgrade request to upstream: {err}"),
        )
    })?;

    let mut buf = BytesMut::with_capacity(4096);
    loop {
        if buf.len() > MAX_HEAD_BYTES {
            return Err(OagwError::new(
                ErrorKind::ProtocolError,
                "upstream upgrade response head exceeds 64 KiB",
            ));
        }
        if let Some(exchange) = try_parse_head(&buf)? {
            let (status, headers, consumed) = exchange;
            let leftover = Bytes::copy_from_slice(&buf[consumed..]);
            return Ok(UpgradeExchange {
                status,
                headers,
                leftover,
                stream,
            });
        }
        let read = stream.read_buf(&mut buf).await.map_err(|err| {
            OagwError::new(
                ErrorKind::StreamAborted,
                format!("upstream closed during the upgrade handshake: {err}"),
            )
        })?;
        if read == 0 {
            return Err(OagwError::new(
                ErrorKind::StreamAborted,
                "upstream closed the connection before completing the upgrade handshake",
            ));
        }
    }
}

/// Parse a response head; `Ok(None)` means "need more bytes".
///
/// # Errors
/// `502 ProtocolError` when the bytes are not a valid HTTP/1.x response head.
#[allow(clippy::type_complexity)]
pub fn try_parse_head(buf: &[u8]) -> OagwResult<Option<(StatusCode, HeaderMap, usize)>> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(buf) {
        Ok(httparse::Status::Complete(consumed)) => {
            let code = response.code.ok_or_else(|| {
                OagwError::new(ErrorKind::ProtocolError, "upstream response has no status")
            })?;
            let status = StatusCode::from_u16(code).map_err(|_| {
                OagwError::new(
                    ErrorKind::ProtocolError,
                    format!("upstream returned an invalid status code {code}"),
                )
            })?;
            let mut map = HeaderMap::new();
            for header in response.headers.iter() {
                if header.name.is_empty() {
                    continue;
                }
                let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(header.name.as_bytes()),
                    HeaderValue::from_bytes(header.value),
                ) else {
                    continue;
                };
                map.append(name, value);
            }
            Ok(Some((status, map, consumed)))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(err) => Err(OagwError::new(
            ErrorKind::ProtocolError,
            format!("malformed upstream response head: {err}"),
        )),
    }
}

/// Pump bytes between the upgraded client connection and the upstream stream
/// until either side closes.
pub async fn splice<C>(mut client: C, mut upstream: Stream, leftover: Bytes)
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    if !leftover.is_empty()
        && let Err(err) = client.write_all(&leftover).await
    {
        tracing::debug!(
            target: "oagw.proxy",
            error = %err,
            "failed to replay buffered upstream bytes to the client"
        );
        return;
    }
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((to_upstream, to_client)) => tracing::debug!(
            target: "oagw.proxy",
            to_upstream,
            to_client,
            "upgraded connection closed"
        ),
        Err(err) => tracing::debug!(
            target: "oagw.proxy",
            error = %err,
            "upgraded connection ended with an error"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_detection_requires_both_headers() {
        let mut headers = HeaderMap::new();
        assert!(!is_upgrade_request(&headers));
        headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        assert!(!is_upgrade_request(&headers));
        headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        assert!(is_upgrade_request(&headers));
    }

    #[test]
    fn request_head_is_well_formed_and_replaces_host() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::HOST, HeaderValue::from_static("ignored"));
        headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(
            http::header::SEC_WEBSOCKET_KEY,
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        let head = encode_request_head("GET", "/chat?room=1", "echo.example.com:8443", &headers);
        let text = String::from_utf8(head).expect("ascii");
        assert!(text.starts_with("GET /chat?room=1 HTTP/1.1\r\n"));
        assert!(text.contains("Host: echo.example.com:8443\r\n"));
        assert_eq!(text.matches("host:").count() + text.matches("Host:").count(), 1);
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn head_parsing_is_incremental() {
        let full = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nEXTRA";
        assert!(try_parse_head(&full[..20]).expect("partial").is_none());

        let (status, headers, consumed) = try_parse_head(full).expect("parse").expect("complete");
        assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(
            headers
                .get(http::header::UPGRADE)
                .and_then(|v| v.to_str().ok()),
            Some("websocket")
        );
        assert_eq!(&full[consumed..], b"EXTRA");
    }

    #[test]
    fn malformed_heads_are_protocol_errors() {
        let err = try_parse_head(b"NOT-HTTP\r\n\r\n").expect_err("malformed");
        assert_eq!(err.status(), 502);
    }
}
