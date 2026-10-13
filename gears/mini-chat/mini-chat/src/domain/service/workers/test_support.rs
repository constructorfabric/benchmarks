#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
//! Test doubles of the background workers: a manual monotonic clock whose
//! sleepers a test releases, and a recording metrics sink.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;

use super::clock::WorkerClock;
use super::metrics::{CleanupResource, SummaryResult, WorkerMetrics};

/// A monotonic clock that only moves when the test advances it.
pub struct ManualClock {
    now: watch::Sender<Duration>,
    sleepers: Mutex<Vec<(u64, Duration)>>,
    next: AtomicU64,
}

/// Removes its sleeper when the sleep future is dropped or completes.
struct SleeperGuard<'a> {
    clock: &'a ManualClock,
    id: u64,
}

impl Drop for SleeperGuard<'_> {
    fn drop(&mut self) {
        self.clock
            .sleepers
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != self.id);
    }
}

impl ManualClock {
    /// A clock at zero.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            now: watch::channel(Duration::ZERO).0,
            sleepers: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        })
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        self.now.send_modify(|now| *now += by);
    }

    /// Moves the clock to `to` (never back).
    pub fn advance_to(&self, to: Duration) {
        self.now.send_modify(|now| {
            if to > *now {
                *now = to;
            }
        });
    }

    /// The sleepers that have not elapsed.
    pub fn pending(&self) -> usize {
        let now = *self.now.borrow();
        self.sleepers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, deadline)| *deadline > now)
            .count()
    }

    /// The earliest deadline of a pending sleeper.
    pub fn next_deadline(&self) -> Option<Duration> {
        let now = *self.now.borrow();
        self.sleepers
            .lock()
            .unwrap()
            .iter()
            .map(|(_, deadline)| *deadline)
            .filter(|deadline| *deadline > now)
            .min()
    }

    /// Waits (real time, 10 s at most) until at least `expected` sleepers are
    /// pending and the count has been the same for two polls 10 ms apart (the
    /// tasks that were released by the previous step have then run and
    /// registered their next sleepers), then moves the clock to the earliest
    /// deadline; returns the new time.
    pub async fn advance_when_pending(&self, expected: usize) -> Duration {
        let mut last = None;
        for _ in 0..1000 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let pending = self.pending();
            if pending >= expected && last == Some(pending) {
                let to = self.next_deadline().expect("a pending sleeper");
                self.advance_to(to);
                return to;
            }
            last = Some(pending);
        }
        panic!(
            "timed out waiting for {expected} pending sleepers (have {})",
            self.pending()
        );
    }
}

#[async_trait]
impl WorkerClock for ManualClock {
    fn now(&self) -> Duration {
        *self.now.borrow()
    }

    async fn sleep(&self, duration: Duration) {
        let deadline = self.now() + duration;
        if duration.is_zero() {
            return;
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.sleepers.lock().unwrap().push((id, deadline));
        let _guard = SleeperGuard { clock: self, id };
        let mut rx = self.now.subscribe();
        rx.wait_for(|now| *now >= deadline)
            .await
            .expect("the clock lives");
    }
}

/// One recorded worker metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    OrphanDetected,
    OrphanFinalized,
    OrphanStreamAborted,
    OrphanScan,
    CleanupCompleted(CleanupResource),
    CleanupFailed(CleanupResource),
    CleanupRetry(CleanupResource),
    CleanupStoreWithFailedAttachments,
    SecondaryCleanupSkipped,
    SummaryExecution(SummaryResult),
    SummaryCasConflict,
    SummaryFallback,
}

/// A [`WorkerMetrics`] that keeps what it was asked to record.
#[derive(Default)]
pub struct RecordingWorkerMetrics {
    events: Mutex<Vec<Metric>>,
    scans: Mutex<Vec<f64>>,
}

impl RecordingWorkerMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Everything recorded, in order.
    pub fn events(&self) -> Vec<Metric> {
        self.events.lock().unwrap().clone()
    }

    /// How often `metric` was recorded.
    pub fn count(&self, metric: Metric) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|recorded| **recorded == metric)
            .count()
    }

    /// The scan durations.
    pub fn scan_seconds(&self) -> Vec<f64> {
        self.scans.lock().unwrap().clone()
    }

    fn push(&self, metric: Metric) {
        self.events.lock().unwrap().push(metric);
    }
}

impl WorkerMetrics for RecordingWorkerMetrics {
    fn orphan_detected(&self) {
        self.push(Metric::OrphanDetected);
    }

    fn orphan_finalized(&self) {
        self.push(Metric::OrphanFinalized);
    }

    fn orphan_stream_aborted(&self) {
        self.push(Metric::OrphanStreamAborted);
    }

    fn orphan_scan_seconds(&self, seconds: f64) {
        self.scans.lock().unwrap().push(seconds);
        self.push(Metric::OrphanScan);
    }

    fn chat_cleanup_completed(&self, resource: CleanupResource) {
        self.push(Metric::CleanupCompleted(resource));
    }

    fn chat_cleanup_failed(&self, resource: CleanupResource) {
        self.push(Metric::CleanupFailed(resource));
    }

    fn chat_cleanup_retry(&self, resource: CleanupResource) {
        self.push(Metric::CleanupRetry(resource));
    }

    fn chat_cleanup_store_with_failed_attachments(&self) {
        self.push(Metric::CleanupStoreWithFailedAttachments);
    }

    fn chat_secondary_cleanup_skipped(&self) {
        self.push(Metric::SecondaryCleanupSkipped);
    }

    fn summary_execution(&self, result: SummaryResult) {
        self.push(Metric::SummaryExecution(result));
    }

    fn summary_cas_conflict(&self) {
        self.push(Metric::SummaryCasConflict);
    }

    fn summary_fallback(&self) {
        self.push(Metric::SummaryFallback);
    }
}
