//! Orphan turn watchdog (DESIGN section 4 "Orphan Turn Watchdog", B.9.1).
//!
//! Each scan (leader only) reads at most [`SCAN_BATCH`] live `running` turns
//! whose `COALESCE(last_progress_at, started_at)` is at or before `now -
//! timeout` (application clock, ADR-0010) and hands each to
//! [`FinalizationService::finalize_orphan`], whose CAS re-checks every
//! predicate; discovery alone never finalizes. A per-turn failure is logged
//! and the scan goes on with the next candidate. The `mini_chat_orphan_*`
//! metrics are recorded as log events (no metrics pipeline in this build).

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use toolkit_db::DBProvider;
use uuid::Uuid;

use super::{LeaderElector, ROLE_ORPHAN_WATCHDOG, SCAN_BATCH};
use crate::domain::error::DomainError;
use crate::domain::services::finalization_service::FinalizationService;
use crate::domain::time::db_ts;
use crate::infra::db::entities::chat_turn;
use crate::infra::db::repos::turn_repo;

pub struct OrphanWatchdog {
    db: Arc<DBProvider<DomainError>>,
    finalization: Arc<FinalizationService>,
    elector: Arc<dyn LeaderElector>,
    timeout: Duration,
}

impl OrphanWatchdog {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        finalization: Arc<FinalizationService>,
        elector: Arc<dyn LeaderElector>,
        timeout: Duration,
    ) -> Self {
        Self {
            db,
            finalization,
            elector,
            timeout,
        }
    }

    /// One scan at `now`; returns the number of turns this scan finalized.
    /// A non-leader scans nothing, and a scan stops taking candidates once
    /// leadership is lost.
    ///
    /// # Errors
    /// Database failure of the candidate query.
    pub async fn scan_once(&self, now: OffsetDateTime) -> Result<u32, DomainError> {
        if !self.leads() {
            return Ok(0);
        }
        let started = std::time::Instant::now();
        let cutoff = db_ts(now - self.timeout);
        let candidates = {
            let conn = self.db.conn()?;
            turn_repo::orphan_candidates(&conn, cutoff, SCAN_BATCH).await?
        };
        let mut finalized = 0u32;
        for turn in &candidates {
            if !self.leads() {
                tracing::info!("orphan watchdog: leadership lost; scan stopped");
                break;
            }
            if self.finalize(turn, cutoff).await {
                finalized += 1;
            }
        }
        tracing::debug!(
            candidates = candidates.len(),
            finalized,
            duration_ms = started.elapsed().as_millis(),
            "orphan watchdog scan (mini_chat_orphan_scan_duration_seconds)"
        );
        Ok(finalized)
    }

    /// Finalizes one candidate; `true` when this scan won its CAS. Failures
    /// are logged (a later scan retries).
    async fn finalize(&self, turn: &chat_turn::Model, cutoff: OffsetDateTime) -> bool {
        log_detected(turn.id);
        let res = self.finalization.finalize_orphan(turn, cutoff).await;
        log_outcome(turn.id, &res);
        matches!(res, Ok(true))
    }

    fn leads(&self) -> bool {
        self.elector.is_leader(ROLE_ORPHAN_WATCHDOG)
    }
}

fn log_detected(turn_id: Uuid) {
    tracing::info!(
        %turn_id,
        reason = "stale_progress",
        "orphan turn detected (mini_chat_orphan_detected_total)"
    );
}

fn log_outcome(turn_id: Uuid, res: &Result<bool, DomainError>) {
    match res {
        Ok(true) => tracing::info!(
            %turn_id,
            reason = "stale_progress",
            "orphan turn finalized (mini_chat_orphan_finalized_total)"
        ),
        // the guarded update matched nothing: no longer a candidate
        Ok(false) => {}
        Err(e) => tracing::warn!(
            %turn_id,
            error = %e,
            "orphan finalization failed; retried by a later scan"
        ),
    }
}
