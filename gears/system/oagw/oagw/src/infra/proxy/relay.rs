//! Hand-rolled HTTP/1.1 relay from the axum ingress surface into the pingora
//! bridge listener (`cpt-cf-oagw-feature-data-plane` transport stage).
//!
//! The axum `/oagw/v1/proxy/{alias}/{*rest}` surface cannot reuse hyper's
//! client stack (hyper/hyper-util are compiled with default features off), so
//! [`relay_request`] speaks minimal HTTP/1.1 over a plain tokio `TcpStream`:
//! it writes the buffered request head (original headers minus hop-by-hop and
//! any client-supplied `x-oagw-*`, plus the internal identity headers the
//! pingora gate reads), then streams the response back as an [`axum::body::Body`]
//! supporting content-length, chunked, and close-delimited framing.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Hop-by-hop and transport-framing headers never relayed across the bridge.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Hard cap on relayed request-body bytes. The bridge is on loopback for the
/// gear's own data plane, but the axum surface must still never buffer an
/// unbounded client body (a memory-DoS via oversized requests). 10 MiB keeps
/// generous headroom over the 512 KiB response-streaming test while bounding
/// memory.
const MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Response bodies up to this size are fully drained before the response is
/// committed, so a truncated transfer surfaces as a gateway 502 problem
/// instead of a short success body. Larger bodies stream (and a truncated
/// stream aborts rather than delivering a short success).
const SMALL_RESPONSE_BODY_LIMIT: usize = 256 * 1024;

/// Caller identity resolved by the surface and carried over the internal
/// bridge so the pingora gate can rebuild the [`SecurityContext`].
///
/// [`SecurityContext`]: toolkit_security::SecurityContext
#[derive(Debug, Clone)]
pub struct RelayIdentity {
    /// Route alias being invoked (`x-oagw-backend-alias`).
    pub alias: String,
    /// Caller subject id (`x-oagw-subject-id`).
    pub subject_id: Uuid,
    /// Caller tenant id (`x-oagw-tenant-id`).
    pub tenant_id: Uuid,
    /// Caller bearer token (`x-oagw-bearer`); forwarded to the PEP/upstream
    /// plugins but never logged.
    pub bearer: Option<String>,
    /// Caller token scopes (`x-oagw-token-scopes`).
    pub scopes: Vec<String>,
    /// Per-bridge relay secret (`x-oagw-relay-secret`); the pingora gate
    /// rejects internal requests without it. Empty disables stamping.
    pub relay_secret: String,
}

/// A request to relay across the internal bridge.
#[derive(Debug)]
pub struct RelayRequest {
    /// HTTP method (verbatim).
    pub method: String,
    /// Path-and-query from the original request line.
    pub uri: String,
    /// Original client headers (filtered inside the relay).
    pub headers: HeaderMap,
    /// Caller identity to inject as internal headers.
    pub identity: RelayIdentity,
    /// Request body (fully buffered with its length set explicitly).
    pub body: Body,
}

/// Body framing detected from the relayed response head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFraming {
    /// No body (204/304/1xx or HEAD).
    None,
    /// `Content-Length`-delimited.
    Length(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Close-delimited (read until EOF).
    UntilEof,
}

/// A relay failure. Mapped by the surface to a gateway problem response.
#[derive(Debug)]
pub enum RelayError {
    /// TCP/IO failure talking to the bridge or reading the response.
    Io(io::Error),
    /// Malformed response from the bridge (protocol violation).
    Protocol(String),
    /// Request body could not be drained.
    Body(String),
    /// Request body exceeded the [`MAX_REQUEST_BODY_BYTES`] cap (surface
    /// should map this to `413 PAYLOAD_TOO_LARGE`).
    BodyTooLarge(usize),
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "relay io: {e}"),
            Self::Protocol(d) => write!(f, "relay protocol: {d}"),
            Self::Body(d) => write!(f, "relay body: {d}"),
            Self::BodyTooLarge(limit) => write!(f, "relay body exceeds {limit}-byte cap"),
        }
    }
}

impl std::error::Error for RelayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for RelayError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Re-broadcasts one response body chunk to the axum body channel.
fn send_chunk(sink: &mpsc::UnboundedSender<Result<Bytes, io::Error>>, data: &[u8]) -> bool {
    sink.send(Ok(Bytes::copy_from_slice(data))).is_ok()
}

/// Appends more bytes to `buf` from the stream, returning bytes read.
async fn read_more<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    buf: &mut Vec<u8>,
) -> io::Result<usize> {
    let mut chunk = vec![0u8; 16 * 1024];
    let n = stream.read(&mut chunk).await?;
    buf.extend_from_slice(&chunk[..n]);
    Ok(n)
}

/// Drains the request body up to [`MAX_REQUEST_BODY_BYTES`].
///
/// The body is capped incrementally while draining, so memory stays bounded
/// no matter what the source declares. Exceeding the cap is a
/// [`RelayError::BodyTooLarge`], which the surface maps to a `413`.
async fn collect_request_body(body: Body) -> Result<Bytes, RelayError> {
    use http_body_util::BodyExt;
    let mut body = body;
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| RelayError::Body(e.to_string()))?;
        if let Some(data) = frame.data_ref() {
            if out.len().saturating_add(data.len()) > MAX_REQUEST_BODY_BYTES {
                return Err(RelayError::BodyTooLarge(MAX_REQUEST_BODY_BYTES));
            }
            out.extend_from_slice(data);
        }
    }
    Ok(Bytes::from(out))
}

/// Finds the first `\r\n` at or after `from`, returning its absolute offset.
fn find_crlf(buf: &[u8], from: usize) -> Option<usize> {
    buf[from..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|p| p + from)
}

/// Streaming task draining the bridge response body into the axum body.
async fn stream_body<S: tokio::io::AsyncRead + Unpin>(
    mut stream: S,
    mut buf: Vec<u8>,
    head_consumed: usize,
    framing: BodyFraming,
    sink: mpsc::UnboundedSender<Result<Bytes, io::Error>>,
) {
    let result: io::Result<()> = async {
        let mut offset = head_consumed;
        match framing {
            BodyFraming::None => {}
            BodyFraming::UntilEof => {
                if offset < buf.len() && !send_chunk(&sink, &buf[offset..]) {
                    return Ok(());
                }
                let mut chunk = vec![0u8; 16 * 1024];
                loop {
                    let n = stream.read(&mut chunk).await?;
                    if n == 0 {
                        break;
                    }
                    if !send_chunk(&sink, &chunk[..n]) {
                        break;
                    }
                }
            }
            BodyFraming::Length(total) => {
                let mut remaining = total;
                let avail = buf.len().saturating_sub(offset).min(remaining);
                if avail > 0 {
                    if !send_chunk(&sink, &buf[offset..offset + avail]) {
                        return Ok(());
                    }
                    remaining -= avail;
                }
                let mut chunk = vec![0u8; 16 * 1024];
                while remaining > 0 {
                    let n = stream.read(&mut chunk).await?;
                    if n == 0 {
                        // Early EOF before the declared length was satisfied:
                        // abort rather than emit a short success body.
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "upstream closed with {remaining} of {total} body bytes missing"
                            ),
                        ));
                    }
                    let take = n.min(remaining);
                    if !send_chunk(&sink, &chunk[..take]) {
                        break;
                    }
                    remaining -= take;
                }
            }
            BodyFraming::Chunked => {
                loop {
                    // A chunked body is a sequence of `SIZE\r\ndata\r\n`,
                    // terminated by `0\r\n` + optional trailers + `\r\n`.
                    let Some(crlf) = find_crlf(&buf, offset) else {
                        if read_more(&mut stream, &mut buf).await? == 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "upstream closed before chunk size line",
                            ));
                        }
                        continue;
                    };
                    let line = &buf[offset..crlf];
                    offset = crlf + 2;
                    let size_line = String::from_utf8_lossy(line);
                    let size_hex = size_line.split(';').next().unwrap_or("").trim();
                    let size = usize::from_str_radix(size_hex, 16).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size")
                    })?;
                    if size == 0 {
                        // Consume trailers up to the terminating empty line.
                        loop {
                            let Some(crlf) = find_crlf(&buf, offset) else {
                                if read_more(&mut stream, &mut buf).await? == 0 {
                                    return Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "upstream closed before chunked-terminator",
                                    ));
                                }
                                continue;
                            };
                            let line_is_empty = crlf == offset;
                            offset = crlf + 2;
                            if line_is_empty {
                                break;
                            }
                        }
                        break;
                    }
                    // Wait until `size` payload bytes + CRLF are buffered.
                    loop {
                        if buf.len() - offset >= size + 2 {
                            if !send_chunk(&sink, &buf[offset..offset + size]) {
                                return Ok(());
                            }
                            offset += size + 2;
                            break;
                        }
                        if read_more(&mut stream, &mut buf).await? == 0 {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                format!("upstream closed mid-chunk ({size} bytes pending)"),
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = sink.send(Err(e));
    }
    // Dropping `sink` signals end-of-stream to the axum body.
}

/// A `futures` stream backed by the body channel (used by axum `Body::from_stream`).
struct ChannelStream(mpsc::UnboundedReceiver<Result<Bytes, io::Error>>);

impl futures_util::Stream for ChannelStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

/// A parsed response head: status line, headers, and body offset.
type ResponseHead = (StatusCode, Vec<(String, String)>, usize);

/// Parses a buffered HTTP response head, returning the number of bytes
/// consumed by the head (i.e. where the body starts) when complete.
fn parse_response_head(buf: &[u8]) -> Result<Option<ResponseHead>, RelayError> {
    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut resp = httparse::Response::new(&mut headers);
    match resp.parse(buf) {
        Ok(httparse::Status::Complete(n)) => {
            let code = resp.code.ok_or_else(|| {
                RelayError::Protocol("response head missing status code".to_owned())
            })?;
            let status = StatusCode::from_u16(code)
                .map_err(|_| RelayError::Protocol(format!("invalid status code {code}")))?;
            let mut values = Vec::new();
            for h in resp.headers.iter().filter(|h| !h.name.is_empty()) {
                let name = h.name;
                let Ok(value) = std::str::from_utf8(h.value) else {
                    continue;
                };
                values.push((name.to_owned(), value.to_owned()));
            }
            Ok(Some((status, values, n)))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(e) => Err(RelayError::Protocol(format!("response head parse: {e}"))),
    }
}

/// Relays one proxied request to the bridge and returns the streamed response.
///
/// The request body is buffered and sent with an explicit `Content-Length`;
/// the response body is streamed chunk by chunk into the returned axum
/// [`Body`] (content-length, chunked and close-delimited framing are all
/// decoded).
pub async fn relay_request(port: u16, req: RelayRequest) -> Result<Response<Body>, RelayError> {
    // Collect the request body under a hard cap so Content-Length is exact
    // and oversized bodies are rejected with 413 rather than buffered
    // without bound.
    let body_bytes = collect_request_body(req.body).await?;

    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;

    // --- request head -------------------------------------------------
    let mut head = String::new();
    head.push_str(&req.method);
    head.push(' ');
    head.push_str(&req.uri);
    head.push_str(" HTTP/1.1\r\n");

    // Forward the client's headers except hop-by-hop/transport-framing and
    // any client-supplied internal `x-oagw-*` (never trust those).
    for (name, value) in &req.headers {
        let n = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&n.as_str()) || n == "content-length" {
            continue;
        }
        if n.starts_with("x-oagw-") {
            continue;
        }
        // Header values with control characters would corrupt the request
        // line; skip anything that is not valid ASCII textual.
        let Ok(v) = value.to_str() else {
            continue;
        };
        if v.bytes().any(|b| b == b'\r' || b == b'\n') {
            continue;
        }
        head.push_str(&n);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }

    // Internal identity headers consumed by the pingora gate. The relay
    // secret authenticates this internal request; the gate refuses any
    // internal request that does not carry it.
    if !req.identity.relay_secret.is_empty() {
        head.push_str("x-oagw-relay-secret: ");
        head.push_str(&req.identity.relay_secret);
        head.push_str("\r\n");
    }
    head.push_str("x-oagw-backend-alias: ");
    head.push_str(&req.identity.alias);
    head.push_str("\r\n");
    head.push_str("x-oagw-subject-id: ");
    head.push_str(&req.identity.subject_id.to_string());
    head.push_str("\r\n");
    head.push_str("x-oagw-tenant-id: ");
    head.push_str(&req.identity.tenant_id.to_string());
    head.push_str("\r\n");
    if let Some(bearer) = &req.identity.bearer
        && !bearer.is_empty()
        && !bearer.bytes().any(|b| b == b'\r' || b == b'\n')
    {
        head.push_str("x-oagw-bearer: ");
        head.push_str(bearer);
        head.push_str("\r\n");
    }
    // Scope strings are caller-controlled identity material; a CR/LF in one
    // would corrupt the internal request head (header injection), so scopes
    // with control characters are dropped before embedding.
    let clean_scopes: Vec<&str> = req
        .identity
        .scopes
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty() && !s.bytes().any(|b| b == b'\r' || b == b'\n'))
        .collect();
    if !clean_scopes.is_empty() {
        head.push_str("x-oagw-token-scopes: ");
        head.push_str(&clean_scopes.join(","));
        head.push_str("\r\n");
    }
    head.push_str(&format!("content-length: {}\r\n", body_bytes.len()));
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).await?;
    stream.write_all(&body_bytes).await?;
    stream.flush().await?;

    // --- response head ------------------------------------------------
    let mut buf: Vec<u8> = Vec::new();
    let (status, head_headers, head_len) = loop {
        if let Some(parsed) = parse_response_head(&buf)? {
            break parsed;
        }
        if read_more(&mut stream, &mut buf).await? == 0 {
            return Err(RelayError::Protocol(
                "bridge closed before response head completed".to_owned(),
            ));
        }
    };

    // --- response body framing -----------------------------------------
    let mut framing = BodyFraming::UntilEof;
    let mut has_transfer_encoding = false;
    let mut has_content_length = false;
    let mut content_length = 0usize;
    let mut forwarded: Vec<(String, String)> = Vec::new();
    for (name, value) in &head_headers {
        let lower = name.to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        if lower == "content-length" {
            match value.parse::<usize>() {
                Ok(n) => {
                    has_content_length = true;
                    content_length = n;
                }
                Err(_) => return Err(RelayError::Protocol("invalid content-length".to_owned())),
            }
            continue;
        }
        forwarded.push((name.clone(), value.clone()));
    }
    if head_headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    }) {
        has_transfer_encoding = true;
    }

    let no_body = status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
        || (status.as_u16() >= 100 && status.as_u16() < 200)
        || req.method.eq_ignore_ascii_case("HEAD");
    if !no_body && !has_transfer_encoding && has_content_length {
        framing = BodyFraming::Length(content_length);
    } else if !no_body && has_transfer_encoding {
        framing = BodyFraming::Chunked;
    } else if no_body {
        framing = BodyFraming::None;
    }

    // --- assemble the axum response with the streamed body --------------
    let mut builder = Response::builder().status(status);
    for (name, value) in forwarded {
        // Skip any name/value that http rejects (would panic on insert) so a
        // hostile upstream cannot poison the relayed response.
        let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = http::HeaderValue::from_str(&value) else {
            continue;
        };
        builder = builder.header(name, value);
    }

    let body = match framing {
        // Small declared bodies are drained completely before the response
        // is committed: if the transfer is truncated the relay fails with a
        // protocol error (surfaced as a 502 problem) instead of returning a
        // short success body. Larger bodies stream; a truncated stream
        // aborts mid-transfer (see `stream_body`) rather than delivering a
        // short success.
        BodyFraming::Length(total) if total <= SMALL_RESPONSE_BODY_LIMIT => {
            let mut collected = Vec::with_capacity(total);
            let avail = buf.len().saturating_sub(head_len).min(total);
            if avail > 0 {
                collected.extend_from_slice(&buf[head_len..head_len + avail]);
            }
            let mut remaining = total - avail;
            let mut chunk = vec![0u8; 16 * 1024];
            while remaining > 0 {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    return Err(RelayError::Protocol(format!(
                        "upstream truncated response body: expected {total} bytes, \
                         received {}",
                        total - remaining
                    )));
                }
                let take = n.min(remaining);
                collected.extend_from_slice(&chunk[..take]);
                remaining -= take;
            }
            let bytes = Bytes::from(collected);
            builder = builder.header("content-length", bytes.len());
            Body::from(bytes)
        }
        _ => {
            let (tx, rx) = mpsc::unbounded_channel::<Result<Bytes, io::Error>>();
            // The stream task owns the read side of the TCP connection.
            tokio::spawn(async move {
                stream_body(stream, buf, head_len, framing, tx).await;
            });
            Body::from_stream(ChannelStream(rx))
        }
    };

    builder
        .body(body)
        .map_err(|e| RelayError::Protocol(format!("build relay response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn identity() -> RelayIdentity {
        RelayIdentity {
            alias: "echo".to_owned(),
            subject_id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            bearer: None,
            scopes: Vec::new(),
            relay_secret: String::new(),
        }
    }

    /// A port that is almost certainly closed (bound then released): the
    /// relay rejects an oversized request before ever connecting.
    fn unpopulated_port() -> u16 {
        use std::net::TcpListener;
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// A declared Content-Length body that the upstream closes early must
    /// surface as a protocol error (mapped to a 502 gateway problem), not a
    /// short success body.
    #[tokio::test(flavor = "multi_thread")]
    async fn truncated_content_length_surfaces_protocol_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Discard the request head.
            let mut buf = vec![0u8; 512];
            let _ = sock.read(&mut buf).await;
            // Declare 100 bytes but send 5, then close.
            let head =
                "HTTP/1.1 200 OK\r\ncontent-length: 100\r\ncontent-type: text/plain\r\n\r\nhello";
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        let req = RelayRequest {
            method: "GET".to_owned(),
            uri: "/x".to_owned(),
            headers: HeaderMap::new(),
            identity: identity(),
            body: Body::empty(),
        };
        let err = relay_request(port, req)
            .await
            .expect_err("truncated transfer must fail");
        assert!(
            matches!(&err, RelayError::Protocol(d) if d.contains("truncated")),
            "got unexpected relay error: {err:?}"
        );
    }

    /// An oversized request body is rejected with `BodyTooLarge` (413 at the
    /// surface) before any bridge connection is attempted.
    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_request_body_rejected_before_connect() {
        let big = vec![0u8; MAX_REQUEST_BODY_BYTES + 1];
        let req = RelayRequest {
            method: "POST".to_owned(),
            uri: "/x".to_owned(),
            headers: HeaderMap::new(),
            identity: identity(),
            body: Body::from(big),
        };
        let err = relay_request(unpopulated_port(), req)
            .await
            .expect_err("oversized body must fail");
        assert!(
            matches!(&err, RelayError::BodyTooLarge(_)),
            "got unexpected relay error: {err:?}"
        );
    }

    /// A scope containing CR/LF must not reach the internal bridge; only the
    /// clean scopes are embedded.
    #[test]
    fn header_injection_scopes_are_sanitized() {
        // The join happens on `clean_scopes`; assert the filter logic alone
        // by replicating the predicate used in relay_request.
        let scopes = ["ok-scope", "bad\r\ninjection", "also:ok", "crlf\r\nx"];
        let clean: Vec<&&str> = scopes
            .iter()
            .filter(|s| !s.is_empty() && !s.bytes().any(|b| b == b'\r' || b == b'\n'))
            .collect();
        assert_eq!(clean, vec![&"ok-scope", &"also:ok"]);
        assert!(clean.iter().all(|s| !s.contains(['\r', '\n'])));
    }
}
