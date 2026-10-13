//! The abandoned-upload reaper (`cpt-cf-mini-chat-algo-attachments-reap-abandoned`):
//! a leader-only worker that fails stale `pending` and `uploaded` rows.

use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use super::leader::{LeaderElector, LeaderRole};
use crate::config::UploadReaperConfig;
use crate::domain::service::attachments::model::StaleUpload;
use crate::domain::service::attachments::ports::{AttachmentMetrics, AttachmentRepository};
use crate::domain::service::quota::service::Clock;

/// Name of the reaper task on the gear's background registry.
pub const UPLOAD_REAPER_TASK: &str = "upload-reaper";

/// Lease the reaper asks the elector for.
pub const UPLOAD_REAPER_LEASE: &str = LeaderRole::UploadReaper.lease_name();

/// Rows taken by one scan.
pub const SCAN_BATCH: u32 = 100;

/// What one scan did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Rows selected.
    pub selected: usize,
    /// Rows moved to `failed` / `upload_abandoned`.
    pub abandoned: usize,
}

/// What the reaper is built from.
pub struct ReaperDeps {
    /// Attachment store.
    pub repo: Arc<dyn AttachmentRepository>,
    /// Leadership.
    pub elector: Arc<dyn LeaderElector>,
    /// Instruments.
    pub metrics: Arc<dyn AttachmentMetrics>,
    /// `upload_reaper.*`.
    pub config: UploadReaperConfig,
    /// Application UTC clock.
    pub clock: Clock,
}

// @cpt-dod:cpt-cf-mini-chat-dod-attachments-upload-reaper:p1
/// The reaper.
pub struct UploadReaper {
    deps: ReaperDeps,
}

impl UploadReaper {
    /// Creates the reaper.
    #[must_use]
    pub fn new(deps: ReaperDeps) -> Self {
        Self { deps }
    }

    /// One scan when this replica is the leader; `None` (nothing read, nothing
    /// written) otherwise.
    pub async fn tick(&self) -> Option<ScanReport> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-role-per-worker
        if !self.deps.elector.is_leader(LeaderRole::UploadReaper) {
            return None;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-role-per-worker
        Some(self.scan_once().await)
    }

    /// One scan: cutoff = now - `stale_after_secs`, at most [`SCAN_BATCH`] rows
    /// oldest first, one CAS transaction per row, metrics after each commit and
    /// `mini_chat_upload_reaper_scan_duration_seconds` per scan.
    pub async fn scan_once(&self) -> ScanReport {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-cutoff
        let started = Instant::now();
        let mut report = ScanReport::default();
        let stale_after = time::Duration::seconds(
            i64::try_from(self.deps.config.stale_after_secs).unwrap_or(i64::MAX),
        );
        let cutoff = (self.deps.clock)() - stale_after;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-cutoff
        match self.deps.repo.stale_uploads(cutoff, SCAN_BATCH).await {
            Err(error) => tracing::error!(%error, "the upload reaper scan failed"),
            Ok(rows) => {
                report.selected = rows.len();
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-for-each-stale
                for row in rows {
                    if self.reap(&row, cutoff).await {
                        report.abandoned += 1;
                    }
                }
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-for-each-stale
            }
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-scan-done
        self.deps
            .metrics
            .reaper_scan_seconds(started.elapsed().as_secs_f64());
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-scan-done
        report
    }

    /// One CAS transaction for a stale row; `true` when this scan abandoned it.
    async fn reap(&self, row: &StaleUpload, cutoff: OffsetDateTime) -> bool {
        match self
            .deps
            .repo
            .abandon(row, cutoff, (self.deps.clock)())
            .await
        {
            Ok(true) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-reaper-metrics
                self.deps.metrics.upload_abandoned(row.status);
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-reaper-metrics
                if let Some(secondary) = &row.secondary_file_id {
                    tracing::warn!(
                        attachment_id = %row.id,
                        secondary_file_id = %secondary,
                        "the secondary copy of an abandoned upload is not deleted"
                    );
                }
                true
            }
            Ok(false) => false,
            Err(error) => {
                tracing::error!(attachment_id = %row.id, %error, "an abandoned upload could not be reaped");
                false
            }
        }
    }

    /// Waits `scan_interval_secs`, then [`Self::tick`]s, until `stop` is
    /// cancelled.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        let interval = Duration::from_secs(self.deps.config.scan_interval_secs.max(1));
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-scan-loop
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }
            self.tick().await;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-reap-abandoned:p1:inst-scan-loop
    }
}

#[cfg(test)]
#[path = "upload_reaper_tests.rs"]
mod tests;
