//! The stream pump of `cpt-cf-oagw-algo-stream-pump`.
//!
//! Realizes `cpt-cf-oagw-feature-streaming`'s transfer: the body of one proxy
//! exchange moved one chunk at a time to the caller's half, and a taken-up
//! upgrade moved as raw bytes in both directions. The pump is one routine for
//! both transfer modes and for both directions, and the mode changes only
//! which halves it reads and writes: the `tunnel` mode reads and writes both,
//! the `incremental` mode reads the upstream half
//! `cpt-cf-oagw-algo-outbound-forward` opened and writes the caller's.
//!
//! The module holds no transport type of its own: it reads and writes the two
//! halves it is handed, and the caller assembles the body it returns into the
//! response the proxy path answers with. It implements
//! `cpt-cf-oagw-principle-no-cache` on this path, because it never holds a
//! complete response body, and it keeps the stream contract, because it
//! inspects no byte: no frame is parsed, no event is rewritten, and no
//! keepalive is injected.

use std::sync::Arc;
use std::time::Instant;

use futures_util::future::Either;
use futures_util::stream::{self, StreamExt};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::data_plane::forward::LiveExchange;
use crate::domain::error::DomainError;
use crate::domain::stream::{StreamOutcome, StreamSession, answer_of};

/// The size of the buffer one tunnel direction reads into, which bounds the
/// bytes a direction holds between its read and its write.
const TUNNEL_BUFFER: usize = 8192;

/// The error a torn-down body carries into the transport, which aborts it
/// rather than ending it cleanly: the caller of an exchange whose head was
/// already committed learns of the teardown from the truncation.
const TORN_DOWN: &str = "the stream was torn down before it ended";

/// The state one item of an incremental body is carried in.
type PumpState = (
    LiveExchange,
    Arc<Mutex<StreamSession>>,
    Option<bytes::Bytes>,
    Disconnect,
);

/// Records the outcome a caller's disconnect leaves a transfer in.
///
/// The caller's half belongs to the transport, which the incremental pump
/// never reads: a caller that stops accepting the body ends the transfer by
/// dropping it, and the pump's state is dropped with it. That drop is the only
/// moment the disconnect is observable, and it is the moment the upstream half
/// is closed too, because the exchange it was carried on is dropped with the
/// body. A transfer that ended any other way has an outcome on the session
/// already, so the drop records nothing over it.
struct Disconnect(Arc<Mutex<StreamSession>>);

impl Drop for Disconnect {
    fn drop(&mut self) {
        let mut session = self.0.lock();
        if session.is_open() {
            session.disconnect();
        }
    }
}

/// Transfers the body of one exchange in the `incremental` mode.
///
/// The first chunk is awaited here under the idle deadline, because the
/// response head is not committed until the caller assembles it, and a stall
/// or an abort before the first byte can still be answered as a whole — the
/// 504 and the 502 the two error answers carry. The stream this returns
/// continues the pump past that point, where a teardown can no longer be
/// answered and instead ends the body mid-transfer.
///
/// # Errors
///
/// Returns the `IdleTimeout` failure when no byte arrives within the idle
/// deadline, and the `StreamAborted` failure when the upstream half fails
/// before the first byte arrives.
#[allow(clippy::result_large_err)]
pub async fn incremental(
    mut live: LiveExchange,
    session: Arc<Mutex<StreamSession>>,
) -> Result<stream::BoxStream<'static, Result<bytes::Bytes, String>>, DomainError> {
    // @cpt-dod:cpt-cf-oagw-dod-stream-sse-forwarding:p1
    // The forwarding this DoD requires is the transfer below: the upstream half
    // is read and the caller's half is written as bytes arrive, each chunk
    // flushed by the transport that yields it, no frame parsed, no event
    // rewritten, no keepalive injected, and the `headers.response` rules and
    // the error-source tag already applied by
    // `cpt-cf-oagw-algo-response-classify` before the body was handed over.
    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-idle-if
    // The idle timer is the only deadline over a body, and the wait for the
    // first chunk is the last wait the exchange can still be answered across.
    // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-idle-if
    let first = match timeout_of(&session, live.chunk()).await {
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-idle-return
        // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-idle-return
        Err(()) => return Err(answer_of(StreamOutcome::Stalled).expect("the stall is answered")),
        // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-idle-return
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-idle-return
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-if
        // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-abort-if
        Ok(Err(_failure)) => {
            // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-return
            // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-abort-return
            return Err(answer_of(StreamOutcome::Aborted).expect("the abort is answered"));
            // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-abort-return
            // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-return
        }
        // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-abort-if
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-if
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-if
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-close
        Ok(Ok(None)) => {
            // An upstream that ends its half before the first byte is a close
            // and not an abort, so the caller receives a completed transfer
            // with an empty body and no error answer at all: the bytes already
            // read are the body's end and the caller's half is closed with it.
            session.lock().upstream_closed();
            live.release().await;
            return Ok(stream::empty().boxed());
        }
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-close
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-if
        Ok(Ok(Some(chunk))) => chunk,
    };
    // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-idle-if
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-idle-if

    // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-moved
    // A byte is counted as moved only once it has been read from one half and
    // written and flushed to the other, which is the event the idle timer
    // measures; the transport writes and flushes each item the pump yields
    // before it reads the next, so a caller that accepts nothing moves no
    // byte and is indistinguishable from an upstream that emits none.
    session
        .lock()
        .record_moved(u64::try_from(first.len()).unwrap_or(u64::MAX));
    // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-moved

    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-client-if
    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-client-close
    // The caller's half is the transport's and the pump never reads it, so a
    // caller that stops accepting the body ends the transfer by dropping it.
    // The guard rides the pump's state for exactly this: its drop is the
    // moment the disconnect is observable, and it closes the upstream half and
    // records the client-disconnect outcome the session carries with it.
    let disconnect = Disconnect(Arc::clone(&session));
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-client-close
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-client-if

    // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-flush
    // The direction the `incremental` mode fixes is one way: the upstream half
    // is read and the caller's is written, each chunk as soon as it is read
    // and never accumulated into a complete response body, which is the
    // implementation of `cpt-cf-oagw-principle-no-cache` this feature delivers.
    Ok(stream::unfold(
        (live, session, Some(first), disconnect),
        |(live, session, pending, disconnect)| async move {
            match pending {
                // The chunk the head wait already read is the body's first
                // item, so the bytes reach the caller in the order and at the
                // cadence the upstream emitted them.
                Some(chunk) => Some((Ok(chunk), (live, session, None, disconnect))),
                None => next_chunk(live, session, disconnect).await,
            }
        },
    )
    .boxed())
    // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-flush
}

/// Reads the next chunk of an incremental body.
///
/// The pump's state is returned with the item so the stream can carry on;
/// `None` ends the body, which the transport treats as a clean end, because
/// an upstream that closed its half finished the response.
async fn next_chunk(
    mut live: LiveExchange,
    session: Arc<Mutex<StreamSession>>,
    disconnect: Disconnect,
) -> Option<(Result<bytes::Bytes, String>, PumpState)> {
    match timeout_of(&session, live.chunk()).await {
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-idle-if
        Err(()) => {
            // The head is committed past the first chunk, so a stall and an
            // abort here are recorded and end the body the transport is
            // sending, which is the only way the caller learns of them.
            // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-idle-return
            session.lock().stalled();
            Some((Err(String::from(TORN_DOWN)), (live, session, None, disconnect)))
            // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-idle-return
        }
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-idle-if
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-if
        Ok(Err(_failure)) => {
            // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-return
            session.lock().abort_transfer();
            Some((Err(String::from(TORN_DOWN)), (live, session, None, disconnect)))
            // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-return
        }
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-if
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-if
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-close
        Ok(Ok(None)) => {
            // The upstream's half ended: the bytes already read are the end of
            // the body, the caller's half is closed by the transport that has
            // no item left to yield, and the outcome is recorded.
            session.lock().upstream_closed();
            live.release().await;
            drop(disconnect);
            None
        }
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-close
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-if
        Ok(Ok(Some(chunk))) => {
            // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-idle-reset
            // The idle timer is reset by the byte that just moved, which is
            // why a healthy stream is never answered for being quiet between
            // events and a stalled one is.
            session
                .lock()
                .record_moved(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            Some((Ok(chunk), (live, session, None, disconnect)))
            // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-idle-reset
        }
    }
}

/// Waits for the next chunk under the idle deadline in force.
///
/// `Err(())` is the deadline expiring; the inner result is the read itself.
async fn timeout_of(
    session: &Mutex<StreamSession>,
    read: impl std::future::Future<Output = Result<Option<bytes::Bytes>, DomainError>>,
) -> Result<Result<Option<bytes::Bytes>, DomainError>, ()> {
    let idle = session.lock().idle_timeout;
    match tokio::time::timeout(idle, read).await {
        Ok(inner) => Ok(inner),
        Err(_elapsed) => Err(()),
    }
}

/// Transfers a taken-up upgrade as a byte tunnel in both directions.
///
/// The two halves are read and written to each other for as long as either
/// moves a byte, framed by nothing and interpreted by nothing. The idle
/// deadline is applied to the absence of traffic in either direction, which is
/// the one timer a tunnel is under. The exchange never reaches the shared
/// client's pool: the two connections are torn down when the tunnel ends, and
/// the outcome is the record the request's execution context carries.
///
/// The upstream half is read and written through the session the send opened,
/// because a 101 turns that session's reader and writer into the
/// close-delimited forms that carry the bytes which belong to no message — and
/// the bytes the upstream sent together with the 101 are the first of them, so
/// a half taken out of the session as a bare socket would drop them.
pub async fn tunnel(
    mut live: LiveExchange,
    session: Arc<Mutex<StreamSession>>,
    caller: hyper::upgrade::Upgraded,
) {
    // The caller's half is the upgrade the HTTP layer handed back, which the
    // tokio half reads and writes through the adapter the two runtimes share.
    let (mut caller_read, mut caller_write) =
        tokio::io::split(hyper_util::rt::TokioIo::new(caller));
    // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-bounded
    // One chunk is held between the halves at any moment: this buffer is what
    // a tunnel read fills and nothing is read again until it has been written
    // and flushed to the other half, and the `incremental` form of the same
    // routine holds its one chunk in the single pending slot its pump state
    // carries. A caller that stops accepting bytes therefore stops the reads
    // that would fill it rather than growing it.
    let mut caller_buffer = vec![0u8; TUNNEL_BUFFER];
    // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-bounded
    // The instant the last byte moved, shared by both directions, which is
    // what the idle deadline is measured against.
    let mut last_moved = Instant::now();

    // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-direction
    // Both directions are read, and whichever has the next byte is written to
    // the other; the mode is what fixes the direction here, and the
    // `incremental` form of the same routine reads only the upstream half.
    let outcome: StreamOutcome = loop {
        let idle = session.lock().idle_timeout;
        let wait = idle.saturating_sub(last_moved.elapsed());
        let direction = tokio::time::timeout(wait, async {
            tokio::select! {
                read = caller_read.read(&mut caller_buffer) => Either::Left(read),
                read = live.chunk() => Either::Right(read),
            }
        })
        .await;
        // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-idle-if
        match direction {
            Err(_elapsed) => {
                // The deadline is measured from the last byte either direction
                // moved, so a direction that was silent while the other moved
                // is not torn down for it.
                // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-idle-return
                if last_moved.elapsed() >= idle {
                    break StreamOutcome::Stalled;
                }
                // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-idle-return
                continue;
            }
            // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-teardown-client-if
            // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-client-if
            // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-client-if
            Ok(Either::Left(read)) => {
                // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-teardown-client
                // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-client-close
                // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-client-return
                match read {
                    // The caller's half ends: the upstream half is closed with
                    // it, because the gateway conveys only the close.
                    Ok(0) => break StreamOutcome::ClientDisconnected,
                    Ok(moved) => {
                        // A write that fails tears the tunnel down with bytes
                        // still expected, which is the mid-flight abort the
                        // flow's abort branch answers 502 for.
                        // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-abort-if
                        // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-abort-return
                        if live.write_upstream(&caller_buffer[..moved])
                            .await
                            .is_err()
                        {
                            break StreamOutcome::Aborted;
                        }
                        // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-abort-return
                        // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-abort-if
                        last_moved = Instant::now();
                        session
                            .lock()
                            .record_moved(u64::try_from(moved).unwrap_or(u64::MAX));
                    }
                    Err(_) => break StreamOutcome::Aborted,
                }
                // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-client-return
                // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-client-close
                // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-teardown-client
            }
            // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-client-if
            // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-client-if
            // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-teardown-client-if
            // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-if
            // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-teardown-upstream-if
            // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-upstream-if
            Ok(Either::Right(read)) => {
                // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-close
                // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-teardown-upstream
                // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-upstream-return
                match read {
                    // The upstream's half ends: the bytes already read are
                    // written to the caller before the caller's half is closed
                    // with it, which is why the write precedes the break.
                    Ok(None) => break StreamOutcome::UpstreamClosed,
                    Ok(Some(chunk)) => {
                        if write_to(&mut caller_write, &chunk).await.is_err() {
                            break StreamOutcome::Aborted;
                        }
                        last_moved = Instant::now();
                        session
                            .lock()
                            .record_moved(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                    }
                    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-if
                    Err(_) => break StreamOutcome::Aborted,
                    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-abort-if
                }
                // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-upstream-return
                // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-teardown-upstream
                // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-close
            }
            // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-upstream-if
            // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-teardown-upstream-if
            // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-upstream-if
        }
        // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-idle-if
    };
    // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-direction

    // @cpt-begin:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-return
    // Both halves are torn down together, because the outcome is the end of
    // the tunnel and no half survives it. The record the request's execution
    // context carries is the outcome the pump returns, which the flow's
    // teardown branches and its abort branch all end in, and no log line, no
    // metric, and no span is emitted for any of them.
    let _ = caller_write.shutdown().await;
    drop(caller_read);
    live.teardown().await;
    match outcome {
        StreamOutcome::ClientDisconnected => session.lock().disconnect(),
        StreamOutcome::UpstreamClosed => session.lock().upstream_closed(),
        StreamOutcome::Stalled => session.lock().stalled(),
        StreamOutcome::Aborted => session.lock().abort_transfer(),
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-pump:p1:inst-sp-return
}

/// Writes one chunk to a tunnel half and flushes it.
async fn write_to<T>(
    half: &mut tokio::io::WriteHalf<T>,
    chunk: &[u8],
) -> std::io::Result<()>
where
    T: tokio::io::AsyncWrite + Unpin,
{
    half.write_all(chunk).await?;
    half.flush().await
}
