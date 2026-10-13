//! Leader-only background workers (DESIGN section 3.2, section 4 "Orphan
//! Turn Watchdog", B.9.1, B.9.5): the orphan watchdog and the upload reaper,
//! the leader elector they run under, and the scan loop that drives them.

use std::future::Future;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::domain::background::Background;
use crate::domain::error::DomainError;

pub mod leader;
pub mod orphan_watchdog;
pub mod upload_reaper;

pub use self::leader::{
    LeaderElector, NoopElector, ROLE_ORPHAN_WATCHDOG, ROLE_UPLOAD_REAPER, build_elector,
};
pub use self::orphan_watchdog::OrphanWatchdog;
pub use self::upload_reaper::UploadReaper;

/// Rows one scan takes at most (fixed, not configurable); the rest are picked
/// up by later scans.
pub const SCAN_BATCH: u64 = 100;

/// Spawns a scan loop into `workers`: `f` runs at start and then every
/// `interval` (`MissedTickBehavior::Delay`, so a slow scan never causes a
/// burst) until `cancel` fires. A failed scan is logged and the loop goes on.
pub fn spawn_worker<F, Fut>(
    workers: &mut JoinSet<()>,
    name: &'static str,
    interval: Duration,
    cancel: CancellationToken,
    mut f: F,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<u32, DomainError>> + Send,
{
    workers.spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let scan = tokio::select! {
                () = cancel.cancelled() => break,
                res = f() => res,
            };
            match scan {
                Ok(0) => {}
                Ok(n) => tracing::debug!(worker = name, processed = n, "scan done"),
                Err(e) => tracing::error!(worker = name, error = %e, "scan failed"),
            }
        }
        tracing::debug!(worker = name, "worker stopped");
    });
}

/// Gear stop: waits for the worker set (it observes the cancelled token) and
/// cancels and waits for the request-spawned background tasks, both at once
/// and each bounded by `budget`, so the whole call ends within `budget` and
/// the caller can still stop the outbox before the lifecycle `stop_timeout`.
/// Workers still running at the deadline are aborted.
pub async fn shutdown_workers(
    workers: &mut JoinSet<()>,
    background: &Background,
    budget: Duration,
) {
    let join = async {
        let joined = tokio::time::timeout(budget, async {
            while let Some(res) = workers.join_next().await {
                if let Err(e) = res {
                    tracing::warn!(error = %e, "mini-chat worker ended abnormally");
                }
            }
        })
        .await;
        if joined.is_err() {
            tracing::warn!("mini-chat workers did not stop in time; aborting them");
            workers.abort_all();
        }
    };
    tokio::join!(join, background.shutdown(budget));
}

#[cfg(test)]
#[path = "workers_tests.rs"]
mod workers_tests;
