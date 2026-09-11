// Created: 2026-09-01 by Constructor Tech
//! The WebSocket relay.
//!
//! `docs/DESIGN.md` §4.3: an upgrade is negotiated end to end rather than
//! terminated. The client's handshake — its `Sec-WebSocket-Key` among other
//! things — reaches the upstream verbatim, and the upstream's `101` head,
//! `Sec-WebSocket-Accept` included, comes back verbatim. From then on the
//! two sockets are spliced byte for byte, so frame boundaries, ping/pong
//! frames and close codes all survive untouched.
//!
//! The handshake is written by hand over a raw socket for exactly that
//! reason: a client library would mint its own `Sec-WebSocket-Key`, and the
//! key the client chose is part of the observable behaviour of the session.

use std::sync::Arc;

use httparse::Status;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::domain::errors::OagwError;
use crate::domain::model::Target;

/// How much of the upstream's response head `httparse` may need.
const HEAD_BUFFER: usize = 8 * 1024;

/// The byte counts `httparse` reports when a parse needs more input.
const PARSE_PARTIAL: usize = 0;

/// A connection to the upstream, plain or TLS.
#[allow(clippy::large_enum_variant)] // the TLS session is only ever spliced
pub enum UpstreamIo {
    /// A plaintext socket.
    Plain(tokio::net::TcpStream),
    /// A TLS session over a plaintext socket.
    Tls(tokio_rustls::client::TlsStream<tokio::net::TcpStream>),
}

impl UpstreamIo {
    /// `true` when the transport is TLS.
    #[must_use]
    pub fn is_secure(&self) -> bool {
        matches!(self, Self::Tls(_))
    }

    /// Close the socket. Used when the upstream declined an upgrade and the
    /// session has nothing left to say.
    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        AsyncWriteExt::shutdown(self).await
    }
}

impl AsyncRead for UpstreamIo {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for UpstreamIo {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// The upstream's response head.
#[derive(Debug)]
pub struct UpstreamHead {
    /// Status code, `101` on a completed handshake.
    pub status: u16,
    /// Reason phrase as the upstream wrote it.
    pub reason: String,
    /// Response headers in arrival order.
    pub headers: Vec<(String, String)>,
}

impl UpstreamHead {
    /// The value of the first header called `name`, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Open the upstream socket for a WebSocket session.
///
/// `head` is the client's request head already, i.e. the request line and the
/// headers the gateway decided to forward. It is written to the socket as it
/// stands, so anything the caller wants the upstream to see must already be
/// in it.
///
/// # Errors
/// Returns `link_unavailable` when the socket cannot be opened or the
/// upstream does not answer with a parseable response head.
pub async fn connect(
    target: &Target,
    head: &[u8],
    tls: Option<&Arc<rustls::ClientConfig>>,
) -> Result<(UpstreamIo, UpstreamHead), OagwError> {
    let addr = (target.host.as_str(), target.port);
    let stream = tokio::time::timeout(crate::domain::TIMEOUT_CONNECT, async {
        tokio::net::TcpStream::connect(addr).await
    })
    .await
    .map_err(|_| OagwError::connection_timeout())?
    .map_err(|error| {
        OagwError::link_unavailable(format!("connect to {} failed: {error}", target.authority()))
    })?;
    let _ = stream.set_nodelay(true);

    let mut io = match (target.secure, tls) {
        (true, Some(config)) => {
            let server_name = rustls_pki_types::ServerName::try_from(target.host.as_str())
                .map_err(|error| {
                    OagwError::invalid_target_host(&format!(
                        "'{}' is not a TLS server name: {error}",
                        target.host
                    ))
                })?
                .to_owned();
            let connector = tokio_rustls::TlsConnector::from(Arc::clone(config));
            UpstreamIo::Tls(
                connector
                    .connect(server_name, stream)
                    .await
                    .map_err(|error| {
                        OagwError::link_unavailable(format!(
                            "TLS handshake with {} failed: {error}",
                            target.authority()
                        ))
                    })?,
            )
        }
        (true, None) => {
            return Err(OagwError::link_unavailable(
                "no TLS trust anchors configured; cannot reach a secure upstream",
            ));
        }
        (false, _) => UpstreamIo::Plain(stream),
    };

    write_all(&mut io, head).await?;
    let response = read_head(&mut io).await?;
    Ok((io, response))
}

/// Splice two bidirectional streams until either side closes.
///
/// Both directions run concurrently; when one half ends the other is given
/// the chance to drain, which is how a half-closed WebSocket session is
/// relayed rather than torn down.
pub async fn relay<A, B>(client: &mut A, upstream: &mut B, idle: std::time::Duration)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(idle, tokio::io::copy_bidirectional(client, upstream)).await;
}

async fn write_all(io: &mut UpstreamIo, mut bytes: &[u8]) -> Result<(), OagwError> {
    while !bytes.is_empty() {
        let written = io.write(bytes).await.map_err(|error| {
            OagwError::link_unavailable(format!("write to upstream failed: {error}"))
        })?;
        if written == PARSE_PARTIAL {
            return Err(OagwError::link_unavailable(
                "the upstream accepted no bytes".to_owned(),
            ));
        }
        bytes = &bytes[written..];
    }
    io.flush()
        .await
        .map_err(|error| OagwError::link_unavailable(format!("flush failed: {error}")))
}

async fn read_head(io: &mut UpstreamIo) -> Result<UpstreamHead, OagwError> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let read = tokio::time::timeout(crate::domain::TIMEOUT_CONNECT, io.read(&mut chunk))
            .await
            .map_err(|_| OagwError::connection_timeout())?
            .map_err(|error| {
                OagwError::link_unavailable(format!("reading the upstream head failed: {error}"))
            })?;
        if read == 0 {
            return Err(OagwError::link_unavailable(
                "the upstream closed before sending a response head",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > HEAD_BUFFER {
            return Err(OagwError::protocol_error(
                "the upstream response head exceeds the 8 KiB limit",
            ));
        }
        if let Some(head) = parse_head(&buffer)? {
            return Ok(head);
        }
    }
}

/// Parse a response head, returning `None` while it is still incomplete.
fn parse_head(bytes: &[u8]) -> Result<Option<UpstreamHead>, OagwError> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(bytes) {
        Ok(Status::Complete(_)) => {
            let head = UpstreamHead {
                status: response.code.unwrap_or_default(),
                reason: response.reason.unwrap_or_default().to_owned(),
                headers: response
                    .headers
                    .iter()
                    .map(|h| (h.name.to_ascii_lowercase(), bytes_of(bytes, h).to_owned()))
                    .collect(),
            };
            Ok(Some(head))
        }
        Ok(Status::Partial) => Ok(None),
        Err(error) => Err(OagwError::protocol_error(format!(
            "the upstream response head is not valid HTTP: {error}"
        ))),
    }
}

fn bytes_of<'a>(buffer: &'a [u8], header: &httparse::Header<'a>) -> &'a str {
    let start = header.value.as_ptr() as usize - buffer.as_ptr() as usize;
    let end = start + header.value.len();
    std::str::from_utf8(&buffer[start..end]).unwrap_or_default()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn raw_head() -> &'static [u8] {
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n"
    }

    #[test]
    fn a_complete_head_parses() {
        let head = parse_head(raw_head()).expect("parse").expect("complete");
        assert_eq!(head.status, 101);
        assert_eq!(head.reason, "Switching Protocols");
        assert_eq!(
            head.header("sec-websocket-accept"),
            Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        assert_eq!(head.headers.len(), 3);
    }

    #[test]
    fn a_partial_head_is_not_an_error() {
        assert!(parse_head(b"HTTP/1.1 101 Switch").expect("parse").is_none());
    }

    #[test]
    fn a_garbage_head_is_a_protocol_error() {
        let err = parse_head(b"not http at all\r\n\r\n").unwrap_err();
        assert_eq!(err.status_value(), 502, "{err}");
    }

    #[test]
    fn header_values_survive_the_offset_arithmetic() {
        let head = parse_head(b"HTTP/1.1 101\r\nX-A: alpha\r\nX-B: beta\r\n\r\n")
            .expect("parse")
            .expect("complete");
        assert_eq!(head.header("X-A"), Some("alpha"));
        assert_eq!(head.header("x-b"), Some("beta"));
    }

    #[test]
    fn a_head_with_no_status_is_an_error() {
        let err = parse_head(b"HTTP/1.1\r\n\r\n").unwrap_err();
        assert_eq!(err.status_value(), 502, "{err}");
    }

    #[tokio::test]
    async fn a_plain_socket_is_not_secure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            socket
        });
        let socket = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let _peer = handle.await.expect("peer");
        assert!(!UpstreamIo::Plain(socket).is_secure());
    }
}
