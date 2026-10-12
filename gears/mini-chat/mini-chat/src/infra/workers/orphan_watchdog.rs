//! Orphan turn watchdog worker (DESIGN section 4 "Orphan Turn Watchdog",
//! B.9.1).
//!
//! Every `orphan_watchdog.scan_interval_secs`, while this process leads the
//! `orphan-watchdog` role, one scan fetches at most 100 candidates
//! ([`OrphanFinalizer::candidates`]) and finalizes each through its own
//! transaction ([`OrphanFinalizer::finalize_orphan`]). A failed transaction
//! is logged and the turn is retried by a later scan.
//!
//! Metrics (prefix `metrics.prefix`, default `mini_chat`):
//! `{prefix}_orphan_detected{reason="stale_progress"}` per candidate,
//! `{prefix}_orphan_finalized{reason="stale_progress"}` per won CAS (after
//! the commit), `{prefix}_orphan_scan_duration` (seconds) per scan.

use std::sync::Arc;
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{LeaderElector, WorkerRole, meter, run_periodic};
use crate::config::OrphanWatchdogConfig;
use crate::domain::orphan::{OrphanFinalizer, OrphanOutcome};
use crate::domain::ports::OrphanCandidate;

/// `reason` label of the orphan counters.
pub const STALE_PROGRESS: &str = "stale_progress";

/// Outcome counts of one scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OrphanScanReport {
    /// Candidates the scan returned.
    pub detected: usize,
    /// Turns finalized as orphans.
    pub finalized: usize,
    /// Candidates whose CAS matched nothing.
    pub skipped: usize,
    /// Candidates whose transaction failed.
    pub failed: usize,
    /// The scan stopped early (cancellation or lost leadership).
    pub interrupted: bool,
}

struct OrphanMetrics {
    detected: Counter<u64>,
    finalized: Counter<u64>,
    scan_duration: Histogram<f64>,
}

impl OrphanMetrics {
    fn new(prefix: &str) -> Self {
        let meter = meter();
        Self {
            detected: meter
                .u64_counter(format!("{prefix}_orphan_detected"))
                .with_description("Orphan turn candidates found by the watchdog")
                .build(),
            finalized: meter
                .u64_counter(format!("{prefix}_orphan_finalized"))
                .with_description("Turns finalized by the orphan watchdog")
                .build(),
            scan_duration: meter
                .f64_histogram(format!("{prefix}_orphan_scan_duration"))
                .with_unit("s")
                .with_description("Duration of one orphan watchdog scan")
                .build(),
        }
    }
}

/// The orphan watchdog (module docs).
pub struct OrphanWatchdog {
    finalizer: Arc<OrphanFinalizer>,
    elector: Arc<dyn LeaderElector>,
    config: OrphanWatchdogConfig,
    metrics: OrphanMetrics,
}

impl std::fmt::Debug for OrphanWatchdog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrphanWatchdog")
            .field("config", &self.config)
            .field("elector", &self.elector)
            .finish_non_exhaustive()
    }
}

impl OrphanWatchdog {
    /// Watchdog over `finalizer` (built with `orphan_watchdog.timeout_secs`),
    /// the elector of [`WorkerRole::OrphanWatchdog`] and the
    /// `orphan_watchdog` section; metrics use `metrics_prefix`.
    #[must_use]
    pub fn new(
        finalizer: Arc<OrphanFinalizer>,
        elector: Arc<dyn LeaderElector>,
        config: &OrphanWatchdogConfig,
        metrics_prefix: &str,
    ) -> Self {
        Self {
            finalizer,
            elector,
            config: config.clone(),
            metrics: OrphanMetrics::new(metrics_prefix),
        }
    }

    /// Scans every `scan_interval_secs` until `cancel` fires; returns at once
    /// when the watchdog is disabled.
    pub async fn run(&self, cancel: &CancellationToken) {
        if !self.config.enabled {
            info!(target: "mini_chat.orphan", "orphan watchdog disabled");
            return;
        }
        info!(
            target: "mini_chat.orphan",
            scan_interval_secs = self.config.scan_interval_secs,
            timeout_secs = self.config.timeout_secs,
            "orphan watchdog started"
        );
        run_periodic(
            WorkerRole::OrphanWatchdog,
            self.config.scan_interval_secs,
            self.elector.as_ref(),
            cancel,
            || async {
                self.scan(cancel).await;
            },
        )
        .await;
    }

    /// One scan: finalizes every candidate until `cancel` fires or
    /// leadership is lost.
    pub async fn scan(&self, cancel: &CancellationToken) -> OrphanScanReport {
        let started = Instant::now();
        let report = self.scan_candidates(cancel).await;
        self.metrics
            .scan_duration
            .record(started.elapsed().as_secs_f64(), &[]);
        if report.finalized > 0 || report.failed > 0 {
            info!(
                target: "mini_chat.orphan",
                detected = report.detected,
                finalized = report.finalized,
                skipped = report.skipped,
                failed = report.failed,
                "orphan watchdog scan done"
            );
        }
        report
    }

    async fn scan_candidates(&self, cancel: &CancellationToken) -> OrphanScanReport {
        let mut report = OrphanScanReport::default();
        let candidates = match self.finalizer.candidates().await {
            Ok(candidates) => candidates,
            Err(err) => {
                warn!(target: "mini_chat.orphan", error = %err, "orphan scan failed");
                return report;
            }
        };
        report.detected = candidates.len();
        for candidate in candidates {
            if cancel.is_cancelled() || !self.elector.is_leader() {
                report.interrupted = true;
                break;
            }
            self.process_candidate(candidate, &mut report).await;
        }
        report
    }

    /// Finalizes one candidate and counts the outcome in `report`.
    async fn process_candidate(&self, candidate: OrphanCandidate, report: &mut OrphanScanReport) {
        let reason = [KeyValue::new("reason", STALE_PROGRESS)];
        self.metrics.detected.add(1, &reason);
        match self.finalizer.finalize_orphan(candidate).await {
            Ok(OrphanOutcome::Finalized(settlement)) => {
                self.metrics.finalized.add(1, &reason);
                report.finalized += 1;
                info!(
                    target: "mini_chat.orphan",
                    turn_id = %candidate.id,
                    chat_id = %candidate.chat_id,
                    settlement = ?settlement,
                    "orphan turn finalized"
                );
            }
            Ok(OrphanOutcome::Skipped) => report.skipped += 1,
            Err(err) => {
                report.failed += 1;
                warn!(
                    target: "mini_chat.orphan",
                    turn_id = %candidate.id,
                    error = %err,
                    "orphan finalization failed; retried by a later scan"
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "orphan_watchdog_tests.rs"]
mod tests;
