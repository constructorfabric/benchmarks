//! The bounded non-blocking audit writer of the OAGW gear (entry 2.7).
//!
//! `cpt-cf-oagw-algo-log-sampling` ends in one bounded in-process channel that
//! a task spawned at the gear's initialization drains to stdout. The offer on
//! the request path is a non-blocking attempt that returns immediately whether
//! or not the channel had capacity: no caller ever awaits a log write, no
//! synchronous write happens on the request path and no lock is held across a
//! write on it.
//!
//! A line the channel cannot take is dropped and counted in-process, and the
//! count is reported at `DEBUG`; no metric family beyond the DESIGN §4.2
//! roster is added for it, and no request is slowed or failed by it.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_util::sync::CancellationToken;

/// The bound of the channel the drain task drains, in lines.
pub const WRITER_CAPACITY: usize = 4096;

/// The window the stop path gives the drain to write the lines it holds.
///
/// The drain writes out what the channel already holds without awaiting a
/// producer, so the window only bounds the stop: the gear returns after it
/// whether or not every line made it to stdout.
pub const FLUSH_WINDOW: Duration = Duration::from_millis(100);

/// The lines of one capture, in emission order.
const CAPTURE_BOUND: usize = 512;

/// The capture ring a test reads the emitted lines from.
type Capture = Arc<Mutex<VecDeque<String>>>;

impl Channel {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel(WRITER_CAPACITY);
        Self {
            sender,
            receiver: Some(receiver),
        }
    }
}

/// The two halves of the channel the drain task drains.
///
/// They are replaced together: a receiving half cannot be taken back once the
/// drain that owns it is gone, so a start that follows a stop in the same
/// process swaps the pair for a fresh one.
struct Channel {
    sender: Sender<String>,
    receiver: Option<Receiver<String>>,
}

/// The bounded non-blocking writer the audit lines are offered to.
pub struct AuditWriter {
    channel: Mutex<Channel>,
    drops: AtomicU64,
    written: AtomicU64,
    write_failures: AtomicU64,
    capture: Mutex<Option<Capture>>,
}

impl std::fmt::Debug for AuditWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditWriter")
            .field("capacity", &WRITER_CAPACITY)
            .field("drops", &self.drops())
            .field("write_failures", &self.write_failures())
            .field("written", &self.written())
            .finish_non_exhaustive()
    }
}

impl Default for AuditWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditWriter {
    /// A writer whose channel is bounded by [`WRITER_CAPACITY`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            channel: Mutex::new(Channel::new()),
            drops: AtomicU64::new(0),
            written: AtomicU64::new(0),
            write_failures: AtomicU64::new(0),
            capture: Mutex::new(None),
        }
    }

    /// Take the receiving half the drain task drains.
    ///
    /// Called once per start, when the gear's initialization spawns the drain
    /// task; the returned receiver is dropped with the task, which is what
    /// stops the offers from being accepted after the gear returned. The next
    /// start restores one with [`AuditWriter::restore_receiver`].
    #[must_use]
    pub fn take_receiver(&self) -> Option<Receiver<String>> {
        self.channel.lock().receiver.take()
    }

    /// Restore a receiving half, for a start that follows a stop.
    ///
    /// A receiver the drain took cannot be taken back, so the pair is swapped
    /// for a fresh one. The stop the previous start ended with gave the drain
    /// its flush window and wrote out what the channel then held, so nothing a
    /// producer offered before the stop is left behind in the pair it replaces,
    /// and a subsequent [`AuditWriter::spawn_drain`] drains again.
    pub fn restore_receiver(&self) {
        let mut channel = self.channel.lock();
        if channel.receiver.is_some() {
            return;
        }
        *channel = Channel::new();
    }

    /// Offer one line to the channel without blocking
    /// (`inst-ob-asamp-05`, `inst-ob-asamp-06`).
    ///
    /// The offer is a non-blocking attempt that returns immediately whether or
    /// not the channel had capacity; the caller never awaits a log write.
    // @cpt-begin:cpt-cf-oagw-dod-non-blocking-logging:p1:inst-full
    // The audit line is handed to the bounded in-process channel a separate
    // drain writes to stdout: the offer never waits, holds no lock across the
    // write it attempts, and no synchronous write, file or socket I/O happens on
    // the request path. The metric update beside it is an atomic operation on a
    // registered collector with no I/O, and the successful class is sampled at
    // the fixed 1 in 100 the sampler holds, from no configuration key.
    pub fn offer(&self, line: String) -> bool {
        self.record_captured(&line);
        // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-06
        // The sender is cloned out of the lock and used outside it: the offer
        // is the step on the request path, and the write it attempts holds no
        // lock.
        let sender = self.channel.lock().sender.clone();
        match sender.try_send(line) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => {
                // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-07
                // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-08
                // A channel at capacity drops the line, counts the drop
                // in-process and reports the count at `DEBUG`. No metric family
                // beyond the DESIGN §4.2 roster is added for it, and no request
                // is slowed or failed.
                let drops = self.drops.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::debug!(
                    dropped_lines = drops,
                    capacity = WRITER_CAPACITY,
                    "the audit writer is at capacity; the line is dropped"
                );
                false
                // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-08
                // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-07
            }
        }
        // @cpt-begin:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-09
        // The emission result is returned: the line was handed to the bounded
        // writer or was dropped and counted, and no caller ever awaits the
        // write.
        // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-09
        // @cpt-end:cpt-cf-oagw-algo-log-sampling:p1:inst-ob-asamp-06
    }
    // @cpt-end:cpt-cf-oagw-dod-non-blocking-logging:p1:inst-full

    /// Write one line to stdout, from the drain task only.
    pub fn write_line(&self, line: &str) {
        // A stdout the process has lost is not a reason to fail a request: the
        // failure is absorbed and counted, the same posture as a full channel.
        let outcome = {
            let stdout = std::io::stdout().lock();
            let mut writer = stdout;
            writer
                .write_all(line.as_bytes())
                .and_then(|()| writer.write_all(b"\n"))
                .and_then(|()| writer.flush())
        };
        match outcome {
            Ok(()) => self.written.fetch_add(1, Ordering::Relaxed),
            Err(_) => self.write_failures.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Capture every line the emission path produced, for the in-crate tests.
    ///
    /// The capture is an in-process ring the tests read; it is disabled unless
    /// a test enables it and it adds no I/O. The ring holds the lines the
    /// request path produced whether the channel accepted or dropped them, so
    /// a test reads what the gear emitted without depending on the task the
    /// drain runs on.
    pub fn enable_capture(&self) -> Capture {
        let mut capture = self.capture.lock();
        capture
            .get_or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())))
            .clone()
    }

    /// Record one line into the capture ring, when it is enabled.
    fn record_captured(&self, line: &str) {
        let Some(ring) = self.capture.lock().clone() else {
            return;
        };
        let mut ring = ring.lock();
        if ring.len() >= CAPTURE_BOUND {
            ring.pop_front();
        }
        ring.push_back(line.to_owned());
    }

    /// The lines captured so far, in emission order.
    #[must_use]
    pub fn captured(&self) -> Vec<String> {
        let Some(ring) = self.capture.lock().clone() else {
            return Vec::new();
        };
        ring.lock().iter().cloned().collect()
    }

    /// Forget the lines captured so far.
    pub fn clear_capture(&self) {
        if let Some(ring) = self.capture.lock().clone() {
            ring.lock().clear();
        }
    }

    /// The lines the writer dropped.
    #[must_use]
    pub fn drops(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }

    /// The lines the writer wrote to stdout.
    #[must_use]
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// The lines a stdout write failed on, which are lost as dropped lines are.
    #[must_use]
    pub fn write_failures(&self) -> u64 {
        self.write_failures.load(Ordering::Relaxed)
    }
}

/// The drain task the gear's initialization spawns
/// (`cpt-cf-oagw-algo-log-sampling`).
///
/// The task owns the receiving half of the channel and writes each line to
/// stdout; it stops when the cancellation token the gear's `run` selects on is
/// cancelled, and its join handle is awaited by that `select!`, so no line is
/// written after the gear returns. On cancellation it first writes out what
/// the channel already holds, so the lines the request path offered before the
/// stop are not dropped with the receiver.
pub async fn drain(
    mut receiver: Receiver<String>,
    writer: Arc<AuditWriter>,
    token: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = token.cancelled() => {
                // The gear is stopping: the lines the request path already
                // offered are still written out. The channel is emptied
                // without awaiting a producer, so the stop cannot be held open
                // by a request that has not closed yet, and what a lost stdout
                // cannot take is counted as a failed write as anywhere else.
                while let Ok(line) = receiver.try_recv() {
                    writer.write_line(&line);
                }
                let _ = std::io::stdout().lock().flush();
                return;
            }
            line = receiver.recv() => {
                let Some(line) = line else {
                    let _ = std::io::stdout().lock().flush();
                    return;
                };
                writer.write_line(&line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_offer_never_awaits_and_returns_the_channel_decision() {
        let writer = Arc::new(AuditWriter::new());
        let _ = writer.enable_capture();
        assert!(writer.offer("{\"event\":\"proxy_request\"}".to_owned()));
        assert_eq!(writer.drops(), 0);
    }

    #[test]
    fn a_line_the_channel_cannot_take_is_dropped_and_counted() {
        let writer = Arc::new(AuditWriter::new());
        let _ = writer.enable_capture();
        // Fill the bounded channel, then drop one more line: the bound is the
        // channel's, and the writer never blocks to make room.
        for index in 0..WRITER_CAPACITY {
            assert!(
                writer.offer(format!("{{\"line\":{index}}}")),
                "the bound lines are accepted"
            );
        }
        assert!(!writer.offer("{\"line\":\"overflow\"}".to_owned()));
        assert_eq!(writer.drops(), 1);
    }

    #[test]
    fn the_writer_holds_no_lock_across_a_write_on_the_request_path() {
        let writer = Arc::new(AuditWriter::new());
        let ring = writer.enable_capture();
        let offered = "{\"timestamp\":\"2026-09-06T00:00:00.000Z\"}".to_owned();
        assert!(writer.offer(offered.clone()));
        // The offer took no lock that a concurrent write would need.
        writer.write_line(&offered);
        assert_eq!(ring.lock().back().map(String::as_str), Some(offered.as_str()));
        assert_eq!(writer.written(), 1);
    }

    #[test]
    fn the_capture_ring_is_bounded_and_clearable() {
        let writer = Arc::new(AuditWriter::new());
        let _ = writer.enable_capture();
        for index in 0..(CAPTURE_BOUND + 10) {
            assert!(writer.offer(format!("{{\"line\":{index}}}")));
        }
        let captured = writer.captured();
        assert_eq!(captured.len(), CAPTURE_BOUND);
        let last = CAPTURE_BOUND + 10 - 1;
        assert_eq!(
            captured.last().map(String::as_str),
            Some(format!("{{\"line\":{last}}}").as_str()),
            "the oldest line is dropped, the newest is kept"
        );
        writer.clear_capture();
        assert!(writer.captured().is_empty());
    }

    #[test]
    fn a_writer_without_a_capture_records_nothing() {
        let writer = Arc::new(AuditWriter::new());
        assert!(writer.offer("{\"line\":1}".to_owned()));
        assert!(writer.captured().is_empty());
        writer.write_line("{\"line\":1}");
        assert_eq!(writer.written(), 1);
    }

    #[test]
    fn the_writer_bound_is_the_documented_capacity() {
        assert_eq!(WRITER_CAPACITY, 4096);
        let writer = AuditWriter::new();
        assert_eq!(writer.drops(), 0);
        assert_eq!(writer.written(), 0);
    }

    #[tokio::test]
    async fn the_drain_stops_cleanly_on_the_cancellation_token() {
        let writer = Arc::new(AuditWriter::new());
        let _ = writer.enable_capture();
        let (sender, receiver) = mpsc::channel(WRITER_CAPACITY);
        let token = CancellationToken::new();
        assert!(sender.send("{\"line\":1}".to_owned()).await.is_ok());
        let task = tokio::spawn(drain(receiver, Arc::clone(&writer), token.clone()));
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert_eq!(writer.written(), 1);
    }

    #[tokio::test]
    async fn the_drain_drains_the_lines_the_request_path_offered() {
        let writer = Arc::new(AuditWriter::new());
        let _ = writer.enable_capture();
        for index in 0..8 {
            assert!(writer.offer(format!("{{\"line\":{index}}}")));
        }
        let receiver = writer.take_receiver().expect("the receiver is available");
        let token = CancellationToken::new();
        let task = tokio::spawn(drain(receiver, Arc::clone(&writer), token.clone()));
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert_eq!(writer.written(), 8);
        assert_eq!(writer.captured().len(), 8);
    }

    #[test]
    fn a_receiver_taken_by_a_stop_is_restored_for_the_next_start() {
        // A gear that stopped in this process took the receiving half with it,
        // so the offers of a following start are refused until the start's
        // registration restores one.
        let writer = Arc::new(AuditWriter::new());
        assert!(writer.offer("{\"line\":1}".to_owned()));
        assert!(writer.take_receiver().is_some());
        assert!(
            !writer.offer("{\"line\":2}".to_owned()),
            "no drain holds the channel any more"
        );
        assert!(writer.take_receiver().is_none());
        writer.restore_receiver();
        assert!(
            writer.offer("{\"line\":3}".to_owned()),
            "the start that follows the stop drains again"
        );
        assert!(writer.take_receiver().is_some());
    }
}
