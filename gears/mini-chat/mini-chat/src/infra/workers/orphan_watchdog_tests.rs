#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Orphan watchdog worker tests over the orphan finalization fixture.

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::domain::orphan::tests::{OrphanFixture, RunningSeed};
use crate::infra::db::repo::tests::TENANT_A;
use crate::infra::workers::NoopElector;
use crate::infra::workers::tests::SwitchElector;

fn config(enabled: bool) -> OrphanWatchdogConfig {
    OrphanWatchdogConfig {
        enabled,
        timeout_secs: 300,
        scan_interval_secs: 1,
    }
}

fn watchdog(fx: &OrphanFixture, elector: Arc<dyn LeaderElector>, enabled: bool) -> OrphanWatchdog {
    OrphanWatchdog::new(
        Arc::clone(&fx.finalizer),
        elector,
        &config(enabled),
        "mini_chat_test",
    )
}

#[tokio::test]
async fn scan_finalizes_stale_turns_and_leaves_progressing_ones() {
    let fx = OrphanFixture::new().await;
    let stale = fx.running(TENANT_A, RunningSeed::stale()).await;
    let live = fx.running(TENANT_A, RunningSeed::progressing()).await;
    let worker = watchdog(&fx, Arc::new(NoopElector), true);

    let report = worker.scan(&CancellationToken::new()).await;

    assert_eq!(
        report,
        OrphanScanReport {
            detected: 1,
            finalized: 1,
            ..OrphanScanReport::default()
        }
    );
    assert_eq!(fx.turn(stale.id).await.state, "failed");
    assert_eq!(fx.turn(live.id).await.state, "running");
    assert_eq!(worker.scan(&CancellationToken::new()).await.detected, 0);
}

#[tokio::test]
async fn lost_leadership_or_cancellation_stops_claiming_candidates() {
    let fx = OrphanFixture::new().await;
    let turn = fx.running(TENANT_A, RunningSeed::stale()).await;
    let elector = SwitchElector::new(false);
    let worker = watchdog(&fx, elector.clone(), true);

    let report = worker.scan(&CancellationToken::new()).await;
    assert!(report.interrupted);
    assert_eq!((report.detected, report.finalized), (1, 0));

    elector.set(true);
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(worker.scan(&cancel).await.interrupted);
    assert_eq!(fx.turn(turn.id).await.state, "running");
}

#[tokio::test]
async fn run_scans_until_cancelled() {
    let fx = OrphanFixture::new().await;
    let turn = fx.running(TENANT_A, RunningSeed::stale()).await;
    let worker = Arc::new(watchdog(&fx, Arc::new(NoopElector), true));
    let cancel = CancellationToken::new();
    let task = {
        let worker = Arc::clone(&worker);
        let cancel = cancel.clone();
        tokio::spawn(async move { worker.run(&cancel).await })
    };

    for _ in 0..200 {
        if fx.turn(turn.id).await.state == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(fx.turn(turn.id).await.state, "failed");
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the watchdog stops on cancellation")
        .unwrap();
}

#[tokio::test]
async fn disabled_watchdog_returns_at_once() {
    let fx = OrphanFixture::new().await;
    let turn = fx.running(TENANT_A, RunningSeed::stale()).await;
    let worker = watchdog(&fx, Arc::new(NoopElector), false);

    tokio::time::timeout(
        Duration::from_secs(1),
        worker.run(&CancellationToken::new()),
    )
    .await
    .expect("a disabled watchdog does not loop");
    assert_eq!(fx.turn(turn.id).await.state, "running");
}
