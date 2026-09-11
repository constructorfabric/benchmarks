// Updated: 2026-09-01 by Constructor Tech
//! Adapting an upgraded connection to the runtime's own I/O traits.
//!
//! hyper hands an upgraded connection back as something implementing *its*
//! `Read`/`Write`; the rest of the Data Plane — and the pingora session the
//! upstream end of the upgrade lives on — speaks tokio's `AsyncRead`/
//! `AsyncWrite`. This is the bridge, so the two ends can be piped together
//! with `tokio::io::copy_bidirectional` rather than a hand-rolled pump.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::rt as hyper_rt;
use tokio::io::ReadBuf;

/// A connection whose I/O is expressed through hyper's traits.
pub struct HyperIo<T>(pub T);

impl<T: hyper_rt::Read + Unpin> tokio::io::AsyncRead for HyperIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // The unfilled region is already initialized, so hyper writing into it
        // cannot observe anything the caller did not already clear. hyper's
        // buffer is its own type and its cursor is only obtainable from one,
        // so a short-lived one is built over that region and dropped before
        // the filled length is reported back.
        let this = self.get_mut();
        let filled = {
            let mut inbound = hyper_rt::ReadBuf::new(buf.initialize_unfilled());
            let cursor = inbound.unfilled();
            match Pin::new(&mut this.0).poll_read(cx, cursor) {
                Poll::Ready(Ok(())) => inbound.filled().len(),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        };
        buf.advance(filled);
        Poll::Ready(Ok(()))
    }
}

impl<T: hyper_rt::Write + Unpin> tokio::io::AsyncWrite for HyperIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A hyper source that yields one fixed chunk and then reports the end of
    /// the stream, recording how it was asked to read.
    struct Chunky(Mutex<Vec<u8>>);

    impl hyper_rt::Read for Chunky {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            mut buf: hyper_rt::ReadBufCursor<'_>,
        ) -> Poll<io::Result<()>> {
            let mut rest = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let n = rest.len().min(unsafe { buf.as_mut() }.len()).min(3);
            let taken: Vec<u8> = rest.drain(..n).collect();
            // `put_slice` both writes and advances, so the bytes land at the
            // front of the cursor and the cursor reports them as filled.
            buf.put_slice(&taken);
            Poll::Ready(Ok(()))
        }
    }

    /// A sink that accepts anything, so the write half can be exercised too.
    pub struct Sink;

    impl hyper_rt::Write for Sink {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn reads_arrive_through_the_adapter() {
        let mut io = HyperIo(Chunky(Mutex::new(b"abcdef".to_vec())));
        let mut out = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut io, &mut out)
            .await
            .unwrap();
        assert_eq!(out, b"abcdef");
    }

    #[tokio::test]
    async fn writes_are_accepted_through_the_adapter() {
        let mut io = HyperIo(Sink);
        tokio::io::AsyncWriteExt::write_all(&mut io, b"hello")
            .await
            .unwrap();
    }
}
