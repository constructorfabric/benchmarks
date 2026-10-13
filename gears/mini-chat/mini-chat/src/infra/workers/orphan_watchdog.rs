//! The orphan watchdog (`cpt-cf-mini-chat-algo-background-workers-run-watchdog`,
//! `cpt-cf-mini-chat-algo-background-workers-finalize-orphan`): a leader-only
//! worker that finalizes `running` turns whose durable progress is stale
//! through the orphan variant of the CAS-guarded finalization.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::leader::{LeaderElector, LeaderRole};
use crate::config::OrphanWatchdogConfig;
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::service::{Clock, QuotaService};
use crate::domain::service::workers::ORPHAN_SCAN_BATCH;
use crate::domain::service::workers::metrics::WorkerMetrics;
use crate::domain::service::workers::orphan::{OrphanCandidate, orphan_audit_event, stale_cutoff};
use crate::domain::service::workers::ports::OrphanStore;

/// Name of the watchdog task on the gear's background registry.
pub const ORPHAN_WATCHDOG_TASK: &str = "orphan-watchdog";

/// What one scan did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Candidates selected.
    pub selected: usize,
    /// Turns this scan finalized.
    pub won: usize,
    /// Candidates another finalizer owned (or whose progress was refreshed).
    pub lost: usize,
    /// Finalizations that failed.
    pub errors: usize,
}

/// What the watchdog is built from.
pub struct WatchdogDeps {
    /// The turn store.
    pub store: Arc<dyn OrphanStore>,
    /// The quota service (the snapshot prefetch and the settlement metrics).
    pub quota: Arc<QuotaService>,
    /// Leadership.
    pub elector: Arc<dyn LeaderElector>,
    /// Instruments.
    pub metrics: Arc<dyn WorkerMetrics>,
    /// `orphan_watchdog.*`.
    pub config: OrphanWatchdogConfig,
    /// Application UTC clock.
    pub clock: Clock,
}

/// The per-scan snapshot cache, keyed by `(tenant_id, requester_user_id,
/// policy_version_applied)`; it holds available snapshots only.
type SnapshotCache = HashMap<(Uuid, Uuid, i64), SettlementSnapshot>;

/// How one candidate ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finalized {
    Won,
    Lost,
    Failed,
}

// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-orphan-watchdog:p1
/// The watchdog.
pub struct OrphanWatchdog {
    deps: WatchdogDeps,
}

impl OrphanWatchdog {
    /// Creates the watchdog.
    #[must_use]
    pub fn new(deps: WatchdogDeps) -> Self {
        Self { deps }
    }

    /// One scan when this replica is the leader of `orphan-watchdog`; `None`
    /// (nothing read, nothing written) otherwise.
    pub async fn tick(&self) -> Option<ScanReport> {
        self.tick_until(&CancellationToken::new()).await
    }

    async fn tick_until(&self, stop: &CancellationToken) -> Option<ScanReport> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-watchdog-loop
        if !self.deps.elector.is_leader(LeaderRole::OrphanWatchdog) {
            return None;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-watchdog-loop
        Some(self.scan_until(stop).await)
    }

    /// One scan: the cutoff is `now` minus `timeout_secs`, at most
    /// [`ORPHAN_SCAN_BATCH`] candidates, one CAS transaction per candidate.
    pub async fn scan_once(&self) -> ScanReport {
        self.scan_until(&CancellationToken::new()).await
    }

    async fn scan_until(&self, stop: &CancellationToken) -> ScanReport {
        let deps = &self.deps;
        let started = Instant::now();
        let mut report = ScanReport::default();
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-watchdog-cutoff
        let cutoff = stale_cutoff((deps.clock)(), deps.config.timeout_secs);
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-watchdog-cutoff
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-select-orphans
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-scan-advisory
        match deps.store.scan(cutoff, ORPHAN_SCAN_BATCH).await {
            Err(error) => tracing::error!(%error, "the orphan watchdog scan failed"),
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-scan-advisory
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-select-orphans
            Ok(candidates) => {
                report.selected = candidates.len();
                let mut snapshots = SnapshotCache::new();
                // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-for-each-orphan
                for candidate in candidates {
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-record-detected
                    deps.metrics.orphan_detected();
                    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-record-detected
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-if-leadership-lost
                    if stop.is_cancelled() || !deps.elector.is_leader(LeaderRole::OrphanWatchdog) {
                        break;
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-if-leadership-lost
                    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-finalize-candidate
                    // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-watchdog-finalizes
                    match self
                        .finalize_candidate(&candidate, cutoff, &mut snapshots)
                        .await
                    {
                        Finalized::Won => report.won += 1,
                        Finalized::Lost => report.lost += 1,
                        Finalized::Failed => report.errors += 1,
                    }
                    // @cpt-end:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-watchdog-finalizes
                    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-finalize-candidate
                }
                // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-for-each-orphan
            }
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-record-scan-duration
        deps.metrics
            .orphan_scan_seconds(started.elapsed().as_secs_f64());
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-run-watchdog:p1:inst-record-scan-duration
        report
    }

    /// The snapshot of the candidate's `policy_version_applied`, fetched at most
    /// once per distinct `(tenant, user, version)` per scan and always before a
    /// transaction opens. The scan crosses tenants and the plugin resolves a
    /// snapshot for `(user, version)` within the user's tenant, so the version
    /// alone never identifies a snapshot. Only an available snapshot is cached:
    /// an `Unavailable` fetch (plugin error or timeout) is retried by the next
    /// candidate of the same key, at the cost of one bounded plugin call per
    /// candidate. `Unavailable` without a fetch when there is no version or no
    /// requester.
    async fn snapshot_for(
        &self,
        candidate: &OrphanCandidate,
        cache: &mut SnapshotCache,
    ) -> SettlementSnapshot {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-prefetch-orphan-snapshot
        let (Some(version), Some(user_id)) = (
            candidate.policy_version_applied,
            candidate.requester_user_id,
        ) else {
            return SettlementSnapshot::Unavailable;
        };
        let key = (candidate.tenant_id, user_id, version);
        if let Some(known) = cache.get(&key) {
            return known.clone();
        }
        let fetched = self
            .deps
            .quota
            .prefetch_settlement_snapshot(user_id, candidate.id, u64::try_from(version).ok())
            .await;
        if matches!(fetched, SettlementSnapshot::Available(_)) {
            cache.insert(key, fetched.clone());
        }
        fetched
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-prefetch-orphan-snapshot
    }

    // @cpt-dod:cpt-cf-mini-chat-dod-background-workers-orphan-finalization:p1
    /// The orphan finalization of one candidate.
    async fn finalize_candidate(
        &self,
        candidate: &OrphanCandidate,
        cutoff: OffsetDateTime,
        snapshots: &mut SnapshotCache,
    ) -> Finalized {
        let deps = &self.deps;
        let snapshot = self.snapshot_for(candidate, snapshots).await;
        let now = (deps.clock)();
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-build-orphan-request
        let request = FinalizeRequest::orphan(candidate.id, cutoff, snapshot, now);
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-build-orphan-request
        let audit = orphan_audit_event(candidate, now);
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-run-orphan-finalize
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-match-orphan-result
        match deps.store.finalize(&request, audit.as_ref()).await {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-case-orphan-lost
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-to-finalized-elsewhere
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-stale-to-progressing
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-orphan-terminal
            Ok(FinalizeResult::Lost) => Finalized::Lost,
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-orphan-terminal
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-stale-to-progressing
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-to-finalized-elsewhere
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-case-orphan-lost
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-case-orphan-won
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-to-orphan-failed
            // @cpt-begin:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-return-recovered
            Ok(FinalizeResult::Won(won)) => {
                deps.metrics.orphan_finalized();
                deps.metrics.orphan_stream_aborted();
                deps.quota.record_finalized(&won);
                tracing::warn!(
                    chat_id = %candidate.chat_id,
                    turn_id = %candidate.id,
                    request_id = %candidate.request_id,
                    "turn finalized by orphan watchdog"
                );
                Finalized::Won
            }
            // @cpt-end:cpt-cf-mini-chat-flow-background-workers-abandoned-turn-recovery:p1:inst-return-recovered
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-orphan-candidate:p1:inst-to-orphan-failed
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-case-orphan-won
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-case-orphan-error
            Err(error) => {
                tracing::error!(
                    chat_id = %candidate.chat_id,
                    turn_id = %candidate.id,
                    request_id = %candidate.request_id,
                    %error,
                    "the orphan finalization failed, the turn stays running"
                );
                Finalized::Failed
            } // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-case-orphan-error
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-match-orphan-result
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-finalize-orphan:p1:inst-run-orphan-finalize
    }

    /// Waits `scan_interval_secs`, then scans while leader, until `stop` is
    /// cancelled.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        let interval = Duration::from_secs(self.deps.config.scan_interval_secs.max(1));
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }
            self.tick_until(&stop).await;
        }
    }
}

#[cfg(test)]
#[path = "orphan_watchdog_tests.rs"]
mod tests;
