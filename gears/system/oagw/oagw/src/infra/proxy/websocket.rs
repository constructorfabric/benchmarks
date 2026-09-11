//! WebSocket relay.
//!
//! The gateway is a dumb pipe after the handshake: it forwards the inbound
//! upgrade request upstream with a real client handshake, copies the upstream's
//! `101` answer back and then relays bytes in both directions without
//! inspecting them. No frame is decoded here, because the tunnel may carry any
//! negotiated sub-protocol the gateway does not understand.

use crate::domain::services::proxy::UpgradedStream;
use hyper::rt::{Read as HyperRead, Write as HyperWrite};
use std::pin::Pin;
use std::task::{Context, Poll};

/// Request headers that name the upgrade and must reach the upstream.
pub const UPGRADE_REQUEST_HEADERS: [&str; 5] = [
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
];

/// Response headers copied from the upstream `101` back to the caller.
///
/// The first two are the ones RFC 6455 requires the answering server to send;
/// a client's handshake is not complete without them.
pub const UPGRADE_RESPONSE_HEADERS: [&str; 5] = [
    "connection",
    "upgrade",
    "sec-websocket-accept",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// Whether a request is asking for an upgrade at all.
#[must_use]
pub fn is_upgrade(method: &str, headers: &http::HeaderMap) -> bool {
    let wants_upgrade = headers
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty());
    let announces = headers
        .get("connection")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"));
    method.eq_ignore_ascii_case("GET") && wants_upgrade && announces
}

/// The upgrade headers present on the inbound request, in wire order.
#[must_use]
pub fn forwarded_request_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    UPGRADE_REQUEST_HEADERS
        .iter()
        .filter_map(|name| {
            headers.get(*name).map(|value| {
                (
                    (*name).to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
        })
        .collect()
}

/// The handshake headers worth copying from the upstream answer.
///
/// `content-length` and `transfer-encoding` are never copied: the tunnel has no
/// body in the HTTP sense.
#[must_use]
pub fn response_handshake_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            UPGRADE_RESPONSE_HEADERS
                .iter()
                .any(|kept| kept.eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect()
}

/// Relays bytes between the two upgraded legs until either side closes.
///
/// Both legs are consumed; the returned pair reports how many bytes moved in
/// each direction (client → upstream, upstream → client). A failure on one leg
/// tears the other down with it, so a client that walks away never leaves an
/// upstream connection dangling.
pub async fn relay(mut downstream: UpgradedStream, mut upstream: UpgradedStream) -> (u64, u64) {
    match tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await {
        Ok((client_to_upstream, upstream_to_client)) => (client_to_upstream, upstream_to_client),
        Err(error) => {
            tracing::debug!(error = %error, "websocket relay ended with an error");
            (0, 0)
        }
    }
}

/// A `hyper` upgraded connection presented as a `tokio` duplex stream.
///
/// `hyper` 1.x exposes its sockets through its own `rt::Read`/`rt::Write`
/// traits, which differ from `tokio`'s in the buffer type; this adapter is the
/// only place the two meet.
pub struct UpgradedIo(pub hyper::upgrade::Upgraded);

impl tokio::io::AsyncRead for UpgradedIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // `hyper` only reads from memory it was told is initialised.
        let unfilled = buf.initialize_unfilled();
        let mut readable = hyper::rt::ReadBuf::new(unfilled);
        let this = self.get_mut();
        match HyperRead::poll_read(Pin::new(&mut this.0), cx, readable.unfilled()) {
            Poll::Ready(Ok(())) => {
                let read = readable.filled().len();
                if read > 0 {
                    buf.advance(read);
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl tokio::io::AsyncWrite for UpgradedIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        HyperWrite::poll_write(Pin::new(&mut self.get_mut().0), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        HyperWrite::poll_flush(Pin::new(&mut self.get_mut().0), cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        HyperWrite::poll_shutdown(Pin::new(&mut self.get_mut().0), cx)
    }
}
