//! Periodic background workers (DESIGN section 3.2 "orphan watchdog",
//! "upload reaper", B.9.1, B.9.5, ADR-0010).
//!
//! - [`leader`] — the [`LeaderElector`] abstraction, the no-op elector and
//!   the Kubernetes Lease elector (`k8s` feature);
//! - [`orphan_watchdog`] — [`OrphanWatchdog`]: finalizes turns abandoned in
//!   `running` ([`crate::domain::orphan`]);
//! - [`upload_reaper`] — [`UploadReaper`]: fails uploads abandoned in
//!   `pending` / `uploaded`.
//!
//! Both workers scan every `scan_interval_secs` (the first scan runs at
//! start), only while their elector reports leadership, take at most
//! [`SCAN_LIMIT`] candidates per scan and stop claiming candidates as soon
//! as the cancellation token fires or leadership is lost. A worker whose
//! section has `enabled = false` returns immediately.

use std::future::Future;
use std::time::Duration;

use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::debug;

pub mod leader;
pub mod orphan_watchdog;
pub mod upload_reaper;

pub use leader::{LeaderElector, NoopElector, WorkerRole, elector_for};
pub use orphan_watchdog::{OrphanScanReport, OrphanWatchdog};
pub use upload_reaper::{ReaperScanReport, UploadReaper};

/// Candidates per scan of every worker (fixed, not configurable).
pub const SCAN_LIMIT: u64 = 100;

/// Runs `scan` every `interval_secs` (at least 1 s; the first tick fires at
/// once) until `cancel` fires, skipping ticks while `elector` does not report
/// leadership. A running scan is awaited before the loop observes the
/// cancellation.
pub async fn run_periodic<F, Fut>(
    role: WorkerRole,
    interval_secs: u64,
    elector: &dyn LeaderElector,
    cancel: &CancellationToken,
    mut scan: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    while next_tick(&mut ticker, cancel).await {
        if elector.is_leader() {
            scan().await;
        } else {
            debug!(target: "mini_chat.workers", role = role.as_str(), "not the leader; scan skipped");
        }
    }
    debug!(target: "mini_chat.workers", role = role.as_str(), "worker stopped");
}

/// Waits for the next tick; `false` once `cancel` fired.
async fn next_tick(ticker: &mut tokio::time::Interval, cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        _ = ticker.tick() => true,
    }
}

/// The meter of the gear's worker metrics.
fn meter() -> opentelemetry::metrics::Meter {
    let scope = opentelemetry::InstrumentationScope::builder("mini-chat").build();
    opentelemetry::global::meter_with_scope(scope)
}

#[cfg(test)]
#[path = "workers_tests.rs"]
pub(crate) mod tests;
