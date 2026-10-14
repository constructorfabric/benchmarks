//! The upload reaper (`cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper`): the
//! leader-only scan (role `mini-chat-upload-reaper`) that fails stale `pending` and `uploaded`
//! attachment rows with `upload_abandoned`, sets `cleanup_status` `pending` and enqueues
//! `attachment_upload_abandoned` in the same transaction when a provider file id exists, and
//! records the reaper metrics after the commit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::config::MiniChatConfig;
use crate::domain::attachments::metrics::AttachmentMetrics;
use crate::domain::attachments::model::AttachmentStatus;
use crate::domain::attachments::reaper::{REAPER_BATCH, UploadReaperStore};
use crate::domain::attachments::store::AttachmentCleanupQueue;
use crate::domain::attachments::timing::Timer;
use crate::domain::error::DomainError;
use crate::domain::quota::Clock;
use crate::infra::leader::{LeaderElector, LeaderRole};

/// `upload_reaper.enabled`, `scan_interval_secs` and `stale_after_secs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaperSettings {
    pub enabled: bool,
    /// Default 60 s (range 1 to 3600).
    pub scan_interval: Duration,
    /// Default 300 s (range 60 to 86400): a row whose `updated_at` is older is stale.
    pub stale_after: Duration,
}

impl ReaperSettings {
    /// The values of the validated gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        Self {
            enabled: config.upload_reaper.enabled,
            scan_interval: Duration::from_secs(config.upload_reaper.scan_interval_secs),
            stale_after: Duration::from_secs(config.upload_reaper.stale_after_secs),
        }
    }
}

/// What one scan did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaperReport {
    /// `false` when this process is not the leader (or the reaper is disabled): nothing was
    /// scanned.
    pub scanned: bool,
    /// The stale rows selected (at most 100).
    pub candidates: usize,
    /// The rows failed with `upload_abandoned` by this scan.
    pub reaped: usize,
    /// The rows whose guarded update affected no row (the upload finished, or the row was
    /// deleted or claimed by chat cleanup).
    pub skipped: usize,
    /// The rows whose transaction failed (a later scan retries them).
    pub failed: usize,
}

/// The collaborators of [`UploadReaper`].
pub struct ReaperDeps {
    pub store: Arc<dyn UploadReaperStore>,
    /// The attachment cleanup queue of the shared outbox.
    pub cleanup: Arc<dyn AttachmentCleanupQueue>,
    pub elector: Arc<dyn LeaderElector>,
    /// The application clock (the cutoff and `updated_at`).
    pub clock: Arc<dyn Clock>,
    /// The wait between two scans.
    pub timer: Arc<dyn Timer>,
    pub metrics: Arc<dyn AttachmentMetrics>,
    pub settings: ReaperSettings,
}

/// The upload reaper worker.
pub struct UploadReaper {
    deps: ReaperDeps,
}

impl UploadReaper {
    /// Create the reaper over its collaborators.
    #[must_use]
    pub fn new(deps: ReaperDeps) -> Self {
        Self { deps }
    }

    /// One scan: nothing when this process is not the leader of the role or the reaper is
    /// disabled; otherwise the cutoff is the application clock minus `stale_after`, at most 100
    /// stale rows oldest first, each reaped in its own transaction, then the scan duration is
    /// recorded.
    ///
    /// # Errors
    ///
    /// A [`DomainError`] when the selection itself fails; a failing row is counted in the report
    /// and does not fail the scan.
    pub async fn scan_once(&self) -> Result<ReaperReport, DomainError> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-disabled
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-return-disabled
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-leader
        if !d.settings.enabled || !d.elector.is_leader(LeaderRole::UploadReaper).await {
            return Ok(ReaperReport::default());
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-leader
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-return-disabled
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-disabled
        let started = Instant::now();
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-cutoff
        let now = d.clock.now();
        let stale_after = time::Duration::try_from(d.settings.stale_after)
            .map_err(|_| DomainError::internal("the stale threshold is out of range"))?;
        let cutoff = now - stale_after;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-cutoff
        let rows = d.store.select_stale(cutoff, REAPER_BATCH).await?;
        let mut report = ReaperReport {
            scanned: true,
            candidates: rows.len(),
            ..ReaperReport::default()
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-each
        for row in rows {
            match d
                .store
                .reap(&row, cutoff, d.clock.now(), Arc::clone(&d.cleanup))
                .await
            {
                Ok(true) => {
                    report.reaped += 1;
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-commit
                    // the transaction committed and the wake fired inside `reap`
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-reaper
                    d.metrics.upload_abandoned(row.status.as_str());
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-reaper
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-pending
                    if row.status == AttachmentStatus::Pending {
                        d.metrics.pending_add(-1);
                    }
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-pending
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-commit
                }
                Ok(false) => report.skipped += 1,
                Err(error) => {
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-row-failed
                    report.failed += 1;
                    tracing::warn!(attachment_id = %row.id, %error, "the upload reaper failed a row; a later scan retries it");
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-row-failed
                }
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-each
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-duration
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-reaper
        d.metrics.reaper_scan_duration(started.elapsed());
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-reaper
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-duration
        Ok(report)
    }

    /// The worker loop: returns at once when the reaper is disabled; otherwise waits
    /// `scan_interval` (through the timer, ending early on cancellation), scans, until `token`
    /// is cancelled.
    pub async fn run(self: Arc<Self>, token: CancellationToken) {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-disabled
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-return-disabled
        if !self.deps.settings.enabled {
            return;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-return-disabled
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-disabled
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-loop
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-return
        loop {
            tokio::select! {
                () = token.cancelled() => break,
                () = self.deps.timer.sleep(self.deps.settings.scan_interval) => {}
            }
            if token.is_cancelled() {
                break;
            }
            if let Err(error) = self.scan_once().await {
                tracing::warn!(%error, "the upload reaper scan failed");
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-return
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-reaper:p1:inst-rp-loop
    }
}

#[cfg(test)]
#[path = "upload_reaper_tests.rs"]
mod upload_reaper_tests;
