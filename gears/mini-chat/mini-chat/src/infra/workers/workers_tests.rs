#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Periodic loop tests: leadership gating and cancellation.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;

/// Elector whose leadership is switched by the test.
#[derive(Debug, Default)]
pub struct SwitchElector(pub AtomicBool);

impl SwitchElector {
    pub fn new(leader: bool) -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(leader)))
    }

    pub fn set(&self, leader: bool) {
        self.0.store(leader, Ordering::SeqCst);
    }
}

impl LeaderElector for SwitchElector {
    fn is_leader(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

async fn ticks_within(elector: &SwitchElector, millis: u64) -> usize {
    let cancel = CancellationToken::new();
    let scans = AtomicUsize::new(0);
    let stopper = cancel.clone();
    let stop = async move {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        stopper.cancel();
    };
    let run = run_periodic(WorkerRole::OrphanWatchdog, 1, elector, &cancel, || async {
        scans.fetch_add(1, Ordering::SeqCst);
    });
    tokio::join!(run, stop);
    scans.load(Ordering::SeqCst)
}

#[tokio::test]
async fn first_scan_runs_at_once_and_stops_on_cancellation() {
    let elector = SwitchElector::new(true);
    let scans = tokio::time::timeout(Duration::from_secs(5), ticks_within(&elector, 200))
        .await
        .expect("the loop stops on cancellation");
    assert_eq!(scans, 1, "first tick immediately, next one after 1 s");
}

#[tokio::test]
async fn no_scan_without_leadership() {
    let elector = SwitchElector::new(false);
    assert_eq!(ticks_within(&elector, 200).await, 0);
}

#[tokio::test]
async fn cancelled_before_start_never_scans() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let scans = AtomicUsize::new(0);
    run_periodic(
        WorkerRole::UploadReaper,
        1,
        &NoopElector,
        &cancel,
        || async {
            scans.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await;
    assert_eq!(scans.load(Ordering::SeqCst), 0);
}
