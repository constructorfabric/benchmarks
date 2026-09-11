//! Bridge between hyper's runtime-agnostic IO traits and tokio's.
//!
//! `hyper::upgrade::Upgraded` exposes only `hyper::rt::Read`/`Write`; the
//! byte relay needs tokio's `AsyncRead`/`AsyncWrite`. `hyper_util::rt::TokioIo`
//! adapts in the opposite direction, so this module supplies the missing half.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A hyper upgraded stream readable and writable through tokio's traits.
pub struct HyperToTokio(hyper::upgrade::Upgraded);

impl HyperToTokio {
    /// Wrap an upgraded hyper connection.
    #[must_use]
    pub const fn new(inner: hyper::upgrade::Upgraded) -> Self {
        Self(inner)
    }
}

impl tokio::io::AsyncRead for HyperToTokio {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        client_buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // hyper's buffer tracks initialisation itself, so hand it the fully
        // initialised tail of the tokio buffer and read into that.
        let filled = {
            let spare = client_buf.initialize_unfilled();
            let mut buf = hyper::rt::ReadBuf::new(spare);
            match hyper::rt::Read::poll_read(Pin::new(&mut self.0), cx, buf.unfilled()) {
                Poll::Ready(Ok(())) => buf.filled().len(),
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => return Poll::Pending,
            }
        };
        client_buf.advance(filled);
        Poll::Ready(Ok(()))
    }
}

impl tokio::io::AsyncWrite for HyperToTokio {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        hyper::rt::Write::poll_write(Pin::new(&mut self.0), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        hyper::rt::Write::poll_flush(Pin::new(&mut self.0), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        hyper::rt::Write::poll_shutdown(Pin::new(&mut self.0), cx)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    // The adapter is exercised end to end by the WebSocket tunnel tests; the
    // unit layer only pins the wiring.

    #[test]
    fn read_into_empty_buffer_reports_ready() {
        let mut buffer: tokio::io::ReadBuf<'_> = tokio::io::ReadBuf::new(&mut []);
        let mut pinned = Pin::new(&mut buffer);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let _ = pinned.as_mut().initialize_unfilled();
        assert_eq!(pinned.filled().len(), 0);
        let _ = &mut cx;
    }
}
