//! The orphan watchdog (slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`,
//! algorithm `cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog`): the leader-only scan
//! that finalizes stale `running` turns as `failed` with `orphan_timeout`, settled on the
//! estimated path with the `aborted` usage event and the `turn_failed` audit event.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::quota::{
    FinalizeOutcome, FinalizeRequest, Finalizer, QuotaService, Settlement, TerminalState,
    TurnTerminalUpdate,
};
use crate::domain::streaming::TurnOutbox;
use crate::domain::streaming::audit::{ORPHAN_TIMEOUT, OrphanTurnFacts, build_orphan_audit};
use crate::infra::leader::{LeaderElector, LeaderRole};
use crate::infra::metrics::{MiniChatMetrics, current_trace_id};
use crate::infra::storage::stream_store::time_storage;
use crate::infra::storage::turn_repo::TurnRepo;

/// At most this many candidates are handled per scan.
pub const SCAN_BATCH: usize = 100;

/// `orphan_watchdog.timeout_secs` and `orphan_watchdog.scan_interval_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogSettings {
    /// A turn whose progress is older than this is stale.
    pub timeout: Duration,
    pub scan_interval: Duration,
}

impl WatchdogSettings {
    /// The values of the validated gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        Self {
            timeout: Duration::from_secs(config.orphan_watchdog.timeout_secs),
            scan_interval: Duration::from_secs(config.orphan_watchdog.scan_interval_secs),
        }
    }
}

/// What one scan did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// `false` when this process is not the leader: nothing was scanned or claimed.
    pub scanned: bool,
    /// The stale candidates found (at most [`SCAN_BATCH`]).
    pub candidates: usize,
    /// The turns finalized by this scan.
    pub finalized: usize,
    /// The turns skipped because their CAS affected no row.
    pub skipped: usize,
    /// The turns whose transaction failed (they stay `running` for a later scan).
    pub failed: usize,
}

/// The orphan watchdog worker.
pub struct OrphanWatchdog {
    quota: Arc<QuotaService>,
    outbox: Arc<dyn TurnOutbox>,
    elector: Arc<dyn LeaderElector>,
    metrics: Arc<MiniChatMetrics>,
    settings: WatchdogSettings,
}

impl OrphanWatchdog {
    /// Create the watchdog over the quota service (database, ledger, usage queue and the
    /// application clock), the audit queue, the leader elector and the metrics.
    #[must_use]
    pub fn new(
        quota: Arc<QuotaService>,
        outbox: Arc<dyn TurnOutbox>,
        elector: Arc<dyn LeaderElector>,
        metrics: Arc<MiniChatMetrics>,
        settings: WatchdogSettings,
    ) -> Self {
        Self {
            quota,
            outbox,
            elector,
            metrics,
            settings,
        }
    }

    /// One scan: skipped when this process is not the leader; otherwise the candidates with
    /// `last_progress_at` (or `started_at` when it is NULL) at most the cutoff (the application
    /// clock minus the timeout), each finalized in its own transaction.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the candidate scan itself fails; a failing candidate is counted in
    /// the report and does not fail the scan.
    pub async fn scan_once(&self) -> Result<ScanReport, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-not-leader
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-skip
        // a process that is not the leader of the role claims nothing
        if !self.elector.is_leader(LeaderRole::OrphanWatchdog).await {
            return Ok(ScanReport::default());
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-skip
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-not-leader
        let started = Instant::now();
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-cutoff
        // the application UTC clock minus the timeout
        let cutoff = self.quota.clock.now() - self.settings.timeout;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-cutoff
        let candidates = {
            let conn = self.quota.db.conn()?;
            TurnRepo::stale_running(
                &conn,
                cutoff,
                SCAN_BATCH as u64,
                time_storage(&self.quota.db),
            )
            .await?
        };
        let mut report = ScanReport {
            scanned: true,
            candidates: candidates.len(),
            ..ScanReport::default()
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-each
        for candidate in candidates {
            // leadership is re-checked before each claim
            if !self.elector.is_leader(LeaderRole::OrphanWatchdog).await {
                break;
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-detected
            self.metrics.orphan_detected();
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-detected
            match self.finalize_candidate(&candidate, cutoff).await {
                Ok(true) => report.finalized += 1,
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-cas-lost
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-skip-turn
                // the turn left `running`, was deleted or made progress after discovery: no
                // settlement, no usage or audit event, no orphan-finalized metric
                Ok(false) => report.skipped += 1,
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-skip-turn
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-cas-lost
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-tx-failed
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-retry-later
                // rolled back: the turn stays `running` and a later scan retries it
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(
                        turn_id = %candidate.turn_id,
                        %error,
                        "the orphan finalization failed; the turn stays running"
                    );
                } // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-retry-later
                  // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-tx-failed
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-each
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-scan-duration
        self.metrics.orphan_scan_duration(started.elapsed());
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-scan-duration
        Ok(report)
    }

    /// Finalize one candidate in its own transaction: the CAS and the settlement of the quota
    /// contract (the CAS re-checks `running`, `deleted_at IS NULL` and the stale progress, the
    /// periods come from `started_at`) and the audit event of the orphan path. `Ok(true)` when
    /// this call won, `Ok(false)` when the CAS was lost.
    async fn finalize_candidate(
        &self,
        turn: &OrphanTurnFacts,
        cutoff: time::OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let now = self.quota.clock.now();
        let finished_at = now.replace_microsecond(now.microsecond()).unwrap_or(now);
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-finalize
        let request = FinalizeRequest {
            tenant_id: turn.tenant_id,
            turn_id: turn.turn_id,
            // `failed` with `orphan_timeout` at the application time, no usage and the durable
            // completed counts of the row
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-to-orphan-failed
            update: TurnTerminalUpdate {
                state: TerminalState::Failed,
                error_code: Some(ORPHAN_TIMEOUT.to_owned()),
                error_detail: None,
                assistant_message_id: None,
                completed_at: finished_at,
            },
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-turn-lifecycle:p1:inst-turn-to-orphan-failed
            finalizer: Finalizer::Orphan {
                stale_cutoff: cutoff,
            },
            usage: None,
            tool_counts: turn.tool_counts,
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-finalize
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-invariants
        // the watchdog never creates or changes `messages` rows, never touches a turn that left
        // `running` or made progress after discovery (the CAS re-checks both) and a late provider
        // result cannot reopen the turn it finalized
        let quota = Arc::clone(&self.quota);
        let outbox = Arc::clone(&self.outbox);
        let facts = turn.clone();
        let settled = self
            .quota
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let settled = quota.finalize_turn(tx, &request).await?;
                    // an unstarted replacement turn of a retry or an edit (NULL reserve columns)
                    // is finalized as `orphan_timeout` without settlement
                    let Settlement { result, wake } = match settled {
                        FinalizeOutcome::AlreadyFinalized => return Ok(None),
                        // @cpt-begin:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-to-failed-orphan
                        FinalizeOutcome::NoSettlement => return Ok(Some((None, Vec::new()))),
                        // @cpt-end:cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup:p1:inst-rts-to-failed-orphan
                        FinalizeOutcome::Settled(settlement) => *settlement,
                    };
                    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-audit
                    // `turn_failed`, `selected_model` equal to the effective model, decision
                    // `unknown`, in the same transaction
                    let audit = build_orphan_audit(&facts, finished_at, current_trace_id());
                    let audit_wake = outbox.enqueue_turn_audit(tx, &audit).await?;
                    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-audit
                    Ok(Some((Some(result), vec![wake, audit_wake])))
                })
            })
            .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-invariants
        let Some((result, wakes)) = settled else {
            return Ok(false);
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-finalized-metrics
        // after the commit: the wakes, then the orphan series
        if let Some(result) = result {
            result.record_after_commit(self.quota.metrics.as_ref());
        }
        for wake in wakes {
            wake.fire();
        }
        self.metrics.orphan_finalized();
        self.metrics.streams_aborted("orphan_timeout");
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-finalized-metrics
        Ok(true)
    }

    /// One scan of the worker loop; a failing scan is logged and the loop goes on.
    async fn scan_and_log(&self) {
        match self.scan_once().await {
            Ok(report) if report.candidates > 0 => {
                tracing::info!(?report, "orphan watchdog scan finished");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "the orphan watchdog scan failed"),
        }
    }

    /// The worker loop: wait `scan_interval` (ending early on cancellation), scan, until
    /// `token` is cancelled.
    pub async fn run(self: Arc<Self>, token: CancellationToken) {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-loop
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-non-blocking
        // a background task: no user-visible request path ever waits for a scan
        loop {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-wait
            tokio::select! {
                () = token.cancelled() => break,
                () = tokio::time::sleep(self.settings.scan_interval) => {}
            }
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-wait
            if token.is_cancelled() {
                break;
            }
            self.scan_and_log().await;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-non-blocking
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-return
        // the token was cancelled: the worker returns
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-return
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-orphan-watchdog:p1:inst-ow-loop
    }
}

#[cfg(test)]
#[path = "orphan_watchdog_tests.rs"]
mod orphan_watchdog_tests;
