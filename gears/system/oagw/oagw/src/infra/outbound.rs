//! Outbound transport to an upstream endpoint.
//!
//! TLS comes from pingora's rustls connector; the HTTP/1.1 session on top of
//! it comes from hyper. Requests are always HTTP/1.1 on the outbound hop —
//! that is the protocol every upstream in scope speaks, and it keeps the
//! streaming / upgrade path uniform.
//!
//! Upgrades are the one exception. hyper's HTTP/1 *client* never hands the
//! socket over: its driver answers every `hyper::upgrade::on` waiter with
//! `ManualUpgrade`, and `Connection::without_shutdown` does not complete after
//! a `101` because the dispatcher's `is_done` is never satisfied by a switched
//! connection. An upgrade is therefore negotiated on the raw transport itself —
//! the gateway writes the request head, reads the response head back, and
//! bridges the two sockets byte for byte for the rest of the session.

use bytes::{Bytes, BytesMut};
use http::{HeaderMap, HeaderName, HeaderValue};
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper_util::rt::TokioIo;
use pingora_core::connectors::TransportConnector;
use pingora_core::protocols::Stream;
use pingora_core::upstreams::peer::HttpPeer;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::error::{DomainError, ErrorKind};

/// Largest upgrade head the gateway will read, in bytes.
const MAX_UPGRADE_HEAD: usize = 64 * 1024;

/// How much of a declined upgrade's body the gateway forwards to the client.
///
/// A refusal is an ordinary HTTP response, but the gateway read it off a raw
/// socket, so it bounds the read rather than streaming it.
const MAX_DECLINED_BODY: usize = 1024 * 1024;

/// An established outbound connection.
pub struct UpstreamConnection {
    sender: SendRequest<axum::body::Body>,
}

impl UpstreamConnection {
    /// Send a request and return the upstream response.
    ///
    /// # Errors
    ///
    /// Errors when the connection is gone or the upstream fails before
    /// producing response headers.
    pub async fn send(
        &mut self,
        request: http::Request<Vec<u8>>,
    ) -> Result<http::Response<Incoming>, DomainError> {
        let (parts, body) = request.into_parts();
        let request = http::Request::from_parts(parts, axum::body::Body::from(body));
        self.sender
            .send_request(request)
            .await
            .map_err(|err| request_error(err, "sending the request to the upstream"))
    }
}

/// An upgrade negotiated with the upstream.
pub struct Upgraded {
    /// The response head. Its body holds the octets read past the head.
    pub response: http::Response<Bytes>,
    /// The transport the rest of the session runs on.
    pub io: Stream,
}

/// The upstream half of an upgraded session.
///
/// It replays whatever the head reader had already pulled off the socket before
/// yielding to the transport, so no byte of the switched session is lost.
pub struct Bridge {
    prefix: Bytes,
    io: Stream,
}

impl Bridge {
    /// Wrap `io`, yielding `prefix` before the socket.
    #[must_use]
    pub fn new(prefix: Bytes, io: Stream) -> Self {
        Self { prefix, io }
    }
}

impl AsyncRead for Bridge {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let len = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix.split_to(len));
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Bridge {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

fn connect_error(err: impl std::fmt::Display, stage: &str) -> DomainError {
    let detail = err.to_string();
    // Only the failure mode is logged — never the request payload.
    tracing::debug!(%stage, error = %detail, "oagw upstream connect failed");
    DomainError::new(
        ErrorKind::LinkUnavailable,
        format!("upstream connect failed: {detail}"),
    )
}

fn request_error(err: impl std::fmt::Display, stage: &str) -> DomainError {
    let detail = err.to_string();
    if detail.contains("timeout") || detail.contains("timed out") {
        return DomainError::new(ErrorKind::RequestTimeout, "upstream request timed out");
    }
    DomainError::new(ErrorKind::Downstream, format!("error {stage}: {detail}"))
}

fn io_error(err: impl std::fmt::Display, stage: &str) -> DomainError {
    let detail = err.to_string();
    tracing::debug!(%stage, error = %detail, "oagw upstream transport failed");
    if detail.contains("timeout") || detail.contains("timed out") {
        return DomainError::new(ErrorKind::RequestTimeout, "upstream request timed out");
    }
    DomainError::new(
        ErrorKind::LinkUnavailable,
        format!("upstream {stage} failed: {detail}"),
    )
}

fn protocol(detail: impl Into<String>) -> DomainError {
    DomainError::new(ErrorKind::Protocol, detail)
}

/// The transport pool shared by every proxy request.
#[derive(Clone)]
pub struct Outbound {
    connector: std::sync::Arc<TransportConnector>,
    connect_timeout: std::time::Duration,
    request_timeout: std::time::Duration,
}

impl std::fmt::Debug for Outbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outbound")
            .field("connect_timeout", &self.connect_timeout)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

impl Outbound {
    /// Build a transport with `pool_size` idle connections per host.
    #[must_use]
    pub fn new(
        pool_size: usize,
        connect_timeout: std::time::Duration,
        request_timeout: std::time::Duration,
    ) -> Self {
        Self {
            connector: std::sync::Arc::new(TransportConnector::new(Some(
                pingora_core::connectors::ConnectorOptions::new(pool_size.max(1)),
            ))),
            connect_timeout,
            request_timeout,
        }
    }

    /// Resolve `host:port` and return the first address.
    ///
    /// # Errors
    ///
    /// Errors with [`ErrorKind::LinkUnavailable`] when DNS resolution fails.
    pub async fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> Result<std::net::SocketAddr, DomainError> {
        let mut addrs = tokio::net::lookup_host((host, port)).await.map_err(|err| {
            DomainError::new(
                ErrorKind::LinkUnavailable,
                format!("failed to resolve {host}: {err}"),
            )
        })?;
        addrs.next().ok_or_else(|| {
            DomainError::new(
                ErrorKind::LinkUnavailable,
                format!("no addresses resolved for {host}"),
            )
        })
    }

    /// Open a raw transport to `endpoint`.
    ///
    /// # Errors
    ///
    /// Errors when the endpoint cannot be resolved or the TLS/TCP handshake
    /// fails, with [`ErrorKind::LinkUnavailable`]; with
    /// [`ErrorKind::ConnectionTimeout`] when the connect budget is exceeded;
    /// with [`ErrorKind::Protocol`] when `endpoint.scheme` is not a
    /// transport this release proxies.
    pub async fn open(
        &self,
        endpoint: &crate::domain::model::Endpoint,
    ) -> Result<Stream, DomainError> {
        if endpoint.scheme == crate::domain::model::Scheme::Grpc {
            return Err(DomainError::new(
                ErrorKind::LinkUnavailable,
                "gRPC upstream endpoints are not proxied by this release",
            ));
        }

        let addr = tokio::time::timeout(
            self.connect_timeout,
            self.resolve(&endpoint.host, endpoint.port),
        )
        .await
        .map_err(|_| {
            DomainError::new(
                ErrorKind::ConnectionTimeout,
                format!("DNS resolution of {} timed out", endpoint.host),
            )
        })??;

        let peer = HttpPeer::new(addr, endpoint.scheme.is_tls(), endpoint.host.clone());
        let connect = self.connector.new_stream(&peer);
        let stream = tokio::time::timeout(self.connect_timeout, connect)
            .await
            .map_err(|_| {
                DomainError::new(
                    ErrorKind::ConnectionTimeout,
                    format!(
                        "connecting to {}:{} timed out",
                        endpoint.host, endpoint.port
                    ),
                )
            })?
            .map_err(|err| {
                connect_error(err, &format!("{}:{:?}", endpoint.host, endpoint.scheme))
            })?;
        Ok(stream)
    }

    /// Establish an HTTP/1.1 connection to `endpoint`.
    ///
    /// # Errors
    ///
    /// As [`Outbound::open`], plus [`ErrorKind::Protocol`] when the HTTP/1.1
    /// handshake on the transport fails.
    pub async fn connect(
        &self,
        endpoint: &crate::domain::model::Endpoint,
    ) -> Result<UpstreamConnection, DomainError> {
        let stream = self.open(endpoint).await?;
        let (sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|err| connect_error(err, "HTTP/1.1 handshake"))?;

        tokio::spawn(async move {
            // Drive the connection to completion; dropping it would close the
            // socket before the response body has been drained.
            if let Err(err) = connection.await {
                tracing::trace!(error = %err, "oagw upstream connection closed with error");
            }
        });

        Ok(UpstreamConnection { sender })
    }

    /// Negotiate a protocol switch with `endpoint`.
    ///
    /// The request is written verbatim onto the transport and the response head
    /// is read back off it; the transport comes back in [`Upgraded::io`] with
    /// whatever the head reader had already consumed in the response's body.
    /// The exchange is bounded by `budget`; the switched session that follows it
    /// is not.
    ///
    /// # Errors
    ///
    /// As [`Outbound::open`], plus [`ErrorKind::Validation`] for a request that
    /// cannot be serialized, [`ErrorKind::Protocol`] for an unreadable response
    /// head, and [`ErrorKind::RequestTimeout`] when `budget` is exceeded.
    pub async fn open_upgrade(
        &self,
        endpoint: &crate::domain::model::Endpoint,
        request: &http::Request<Vec<u8>>,
        budget: std::time::Duration,
    ) -> Result<Upgraded, DomainError> {
        let head = request_head(request)?;
        let mut io = self.open(endpoint).await?;

        let exchange = async {
            io.write_all(&head)
                .await
                .map_err(|err| io_error(err, "writing the upgrade request"))?;
            io.flush()
                .await
                .map_err(|err| io_error(err, "writing the upgrade request"))?;
            let (head, rest) = read_upgrade_head(&mut io).await?;
            parse_upgrade_head(&head, rest)
        };

        let response =
            tokio::time::timeout(budget, exchange)
                .await
                .map_err(|_| {
                    DomainError::new(
                        ErrorKind::RequestTimeout,
                        "the upstream did not answer the protocol switch in time",
                    )
                })??;

        let mut response = response;
        if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
            read_declined_body(&mut io, &mut response).await?;
        }
        Ok(Upgraded { response, io })
    }

    /// Request timeout configured on this transport.
    #[must_use]
    pub const fn request_timeout(&self) -> std::time::Duration {
        self.request_timeout
    }
}

/// Serialize the head of `request` for the wire.
///
/// # Errors
///
/// [`ErrorKind::Validation`] when the request carries a body — an upgrade
/// cannot carry one — or a header value that is not wire-representable text.
fn request_head(request: &http::Request<Vec<u8>>) -> Result<Vec<u8>, DomainError> {
    if !request.body().is_empty() {
        return Err(DomainError::new(
            ErrorKind::Validation,
            "an upgrade request cannot carry a body",
        ));
    }
    let uri = request.uri();
    let target = match uri.query() {
        Some(query) => format!("{}?{}", uri.path(), query),
        None => uri.path().to_owned(),
    };

    let mut head = Vec::with_capacity(256);
    head.extend_from_slice(request.method().as_str().as_bytes());
    head.push(b' ');
    head.extend_from_slice(target.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\n");
    for (name, value) in request.headers() {
        let value = value.to_str().map_err(|_| {
            DomainError::new(
                ErrorKind::Validation,
                format!("header {name} is not valid text and cannot be forwarded"),
            )
        })?;
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    Ok(head)
}

/// Offset of the blank line that ends an HTTP head.
fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Read a response head off `io`, returning it and whatever followed it.
///
/// # Errors
///
/// [`ErrorKind::Protocol`] past the head ceiling, [`ErrorKind::LinkUnavailable`]
/// when the upstream hangs up first.
async fn read_upgrade_head(io: &mut Stream) -> Result<(Bytes, Bytes), DomainError> {
    let mut buf = BytesMut::with_capacity(1024);
    loop {
        if let Some(end) = head_end(&buf) {
            let head = buf.split_to(end + 4).freeze();
            return Ok((head, buf.freeze()));
        }
        if buf.len() > MAX_UPGRADE_HEAD {
            return Err(protocol(
                "the upstream upgrade response head exceeds the size limit",
            ));
        }
        let read = io
            .read_buf(&mut buf)
            .await
            .map_err(|err| io_error(err, "reading the upgrade response head"))?;
        if read == 0 {
            return Err(protocol(
                "the upstream closed the connection during the protocol switch",
            ));
        }
    }
}

/// Parse a response head into a [`http::Response`], with `rest` as its body.
///
/// # Errors
///
/// [`ErrorKind::Protocol`] when the head is not a readable HTTP/1.x response.
fn parse_upgrade_head(head: &[u8], rest: Bytes) -> Result<http::Response<Bytes>, DomainError> {
    let text = std::str::from_utf8(head)
        .map_err(|_| protocol("the upstream upgrade response head is not valid UTF-8"))?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut fields = status_line.splitn(3, ' ');
    let version = match fields.next() {
        Some("HTTP/1.0") => http::Version::HTTP_10,
        Some("HTTP/1.1") => http::Version::HTTP_11,
        other => {
            return Err(protocol(format!(
                "the upstream answered the protocol switch with an unreadable status line {other:?}"
            )))
        }
    };
    let code = fields
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| protocol("the upstream upgrade response carries no status code"))?;
    let status = http::StatusCode::from_u16(code)
        .map_err(|err| protocol(format!("the upstream status code is not valid: {err}")))?;

    let mut builder = http::Response::builder().status(status).version(version);
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(protocol(format!(
                "the upstream upgrade response carries a malformed header {line:?}"
            )));
        };
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|err| protocol(format!("the upstream header name is not valid: {err}")))?;
        let value = HeaderValue::from_bytes(value.trim().as_bytes())
            .map_err(|err| protocol(format!("the upstream header value is not valid: {err}")))?;
        builder = builder.header(name, value);
    }

    builder.body(rest).map_err(|err| {
        protocol(format!("the upstream upgrade response is malformed: {err}"))
    })
}

/// Finish reading a declined switch, so the client sees the whole response.
///
/// The gateway read the head off a raw socket, so the body has to be collected
/// by hand: exactly `Content-Length` octets when one is declared, a decoded
/// chunked body when the upstream framed it that way, otherwise to the end of
/// the connection.
///
/// # Errors
///
/// [`ErrorKind::LinkUnavailable`] when the transport fails or the upstream
/// hangs up mid-body.
async fn read_declined_body(
    io: &mut Stream,
    response: &mut http::Response<Bytes>,
) -> Result<(), DomainError> {
    let declared = declared_body_len(response.headers());
    let framing = response
        .headers()
        .get(http::header::TRANSFER_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let closes = response
        .headers()
        .get(http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("close"));

    let mut body = BytesMut::new();
    body.extend_from_slice(response.body());
    loop {
        if let Some(len) = declared {
            if body.len() >= len {
                body.truncate(len);
                break;
            }
        } else if let Some(decoded) = decode_chunked(&body) {
            *response.body_mut() = decoded;
            return Ok(());
        } else if !(closes || framing.is_some()) {
            // No framing at all: the body is whatever arrived with the head.
            break;
        }
        // A chunked body is only complete once its terminator is on the wire;
        // a closing body only once the upstream hangs up.
        if body.len() > MAX_DECLINED_BODY {
            return Err(protocol("the upstream response body exceeds the size limit"));
        }
        let read = io
            .read_buf(&mut body)
            .await
            .map_err(|err| io_error(err, "reading the upgrade response body"))?;
        if read == 0 {
            // The upstream hung up: whatever is here is all there is.
            if let Some(len) = declared {
                body.truncate(len);
            }
            break;
        }
    }
    *response.body_mut() = body.freeze();
    Ok(())
}

/// The `Content-Length` an upstream head declared, if it did.
fn declared_body_len(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
}

/// Decode a chunked body that has already been read in full.
///
/// `None` when `raw` is not a complete chunked body.
#[must_use]
pub fn decode_chunked(raw: &[u8]) -> Option<Bytes> {
    let mut out = Vec::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        let end = rest.windows(2).position(|window| window == b"\r\n")?;
        let size_text = std::str::from_utf8(&rest[..end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[end + 2..];
        if size == 0 {
            return Some(Bytes::from(out));
        }
        if rest.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        if &rest[size..size + 2] != b"\r\n" {
            return None;
        }
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_errors_classify_timeouts() {
        let err = request_error(
            std::io::Error::new(std::io::ErrorKind::TimedOut, "operation timed out"),
            "sending the request to the upstream",
        );
        assert_eq!(err.kind(), ErrorKind::RequestTimeout);
    }

    #[test]
    fn transport_errors_map_to_link_unavailable() {
        let err = connect_error(
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"),
            "connect",
        );
        assert_eq!(err.kind(), ErrorKind::LinkUnavailable);
    }

    #[tokio::test]
    async fn resolving_an_unknown_host_fails() {
        let outbound = Outbound::new(
            1,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        let result = outbound.resolve("oagw-does-not-exist.invalid", 443).await;
        assert!(result.is_err());
    }

    #[test]
    fn a_request_head_is_serialized_verbatim() {
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri("/echo?a=1")
            .header("Host", "upstream:9099")
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Vec::new())
            .unwrap();
        let head = String::from_utf8(request_head(&request).unwrap()).unwrap();
        assert_eq!(
            head,
            "GET /echo?a=1 HTTP/1.1\r\n\
             host: upstream:9099\r\n\
             upgrade: websocket\r\n\
             connection: Upgrade\r\n\
             sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             \r\n"
        );
    }

    #[test]
    fn a_request_head_refuses_a_body() {
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri("/x")
            .body(b"payload".to_vec())
            .unwrap();
        let err = request_head(&request).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
    }

    #[test]
    fn a_response_head_is_parsed_into_a_response() {
        let head = b"HTTP/1.1 101 Switching Protocols\r\n\
                     Upgrade: websocket\r\n\
                     Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
                     \r\n";
        let response = parse_upgrade_head(head, Bytes::from_static(b"leftover")).unwrap();
        assert_eq!(response.status(), http::StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(response.version(), http::Version::HTTP_11);
        assert_eq!(response.headers()["upgrade"], "websocket");
        assert_eq!(response.body().as_ref(), b"leftover");
    }

    #[test]
    fn a_head_with_a_non_standard_reason_still_parses() {
        let head = b"HTTP/1.1 426 Upgrade Required\r\n\r\n";
        let response = parse_upgrade_head(head, Bytes::new()).unwrap();
        assert_eq!(response.status(), http::StatusCode::UPGRADE_REQUIRED);
    }

    #[test]
    fn an_unreadable_status_line_is_a_protocol_error() {
        let err = parse_upgrade_head(b"NOT HTTP\r\n\r\n", Bytes::new()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Protocol);
    }

    #[test]
    fn a_malformed_header_is_a_protocol_error() {
        let err = parse_upgrade_head(b"HTTP/1.1 101 Switching Protocols\r\nnope\r\n\r\n", Bytes::new())
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Protocol);
    }

    #[test]
    fn a_head_is_found_at_its_terminator() {
        assert_eq!(head_end(b"HTTP/1.1 101 X\r\n\r\n"), Some(14));
        assert_eq!(head_end(b"HTTP/1.1 101 X\r\n"), None);
    }

    #[test]
    fn a_complete_chunked_body_is_decoded() {
        let decoded = decode_chunked(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n").unwrap();
        assert_eq!(decoded.as_ref(), b"hello world");
    }

    #[test]
    fn an_incomplete_chunked_body_is_not_decoded() {
        assert!(decode_chunked(b"5\r\nhello\r\n").is_none());
    }

    #[test]
    fn a_chunk_extension_does_not_confuse_the_decoder() {
        let decoded = decode_chunked(b"4;ext=1\r\nabcd\r\n0\r\n\r\n").unwrap();
        assert_eq!(decoded.as_ref(), b"abcd");
    }
}
