//! The upload reaper (DESIGN B.9.5): fails attachments left `pending` or `uploaded` by an upload
//! whose request was dropped or whose process died, and hands the provider file of such a row to
//! the attachment cleanup queue.

use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::model::STATUS_PENDING;
use super::ports::{StaleUpload, UploadReaperRepository};
use crate::domain::cleanup::{AttachmentCleanupEvent, AttachmentCleanupPayload};
use crate::domain::metrics::Metrics;
use crate::domain::orphan::LeaderElector;

/// Rows one scan takes at most (fixed, not configurable).
const SCAN_LIMIT: u64 = 100;

/// The result of one scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaperReport {
    /// Rows the scan found.
    pub found: usize,
    /// Rows this scan failed as abandoned.
    pub reaped: usize,
}

/// The upload reaper.
pub struct UploadReaper {
    repo: Arc<dyn UploadReaperRepository>,
    metrics: Arc<dyn Metrics>,
    stale_after: Duration,
    scan_interval: Duration,
}

impl UploadReaper {
    /// Creates the reaper: `stale_after` is `upload_reaper.stale_after_secs`, `scan_interval` is
    /// `upload_reaper.scan_interval_secs`.
    #[must_use]
    pub fn new(
        repo: Arc<dyn UploadReaperRepository>,
        metrics: Arc<dyn Metrics>,
        stale_after: Duration,
        scan_interval: Duration,
    ) -> Self {
        Self {
            repo,
            metrics,
            stale_after,
            scan_interval,
        }
    }

    /// Scans every `scan_interval` while this process is the leader, until `cancel` fires.
    pub async fn run(&self, elector: Arc<dyn LeaderElector>, cancel: CancellationToken) {
        info!(
            stale_after_secs = self.stale_after.as_secs(),
            scan_interval_secs = self.scan_interval.as_secs(),
            "the upload reaper started"
        );
        while self.wait_for_next_scan(&cancel).await {
            self.scan_as_leader(elector.as_ref()).await;
        }
        info!("the upload reaper stopped");
    }

    /// Sleeps one scan interval; `false` when cancelled meanwhile.
    async fn wait_for_next_scan(&self, cancel: &CancellationToken) -> bool {
        tokio::select! {
            () = cancel.cancelled() => false,
            () = tokio::time::sleep(self.scan_interval) => true,
        }
    }

    async fn scan_as_leader(&self, elector: &dyn LeaderElector) {
        if !elector.is_leader() {
            debug!("not the upload reaper leader; skipping the scan");
            return;
        }
        let report = self.scan_once(crate::domain::clock::now()).await;
        if report.found > 0 {
            info!(
                found = report.found,
                reaped = report.reaped,
                "upload reaper scan finished"
            );
        }
    }

    /// One scan with `now` as the application clock.
    pub async fn scan_once(&self, now: OffsetDateTime) -> ReaperReport {
        let started = Instant::now();
        let mut report = ReaperReport::default();
        let cutoff = now - self.stale_after;
        match self.repo.stale_uploads(cutoff, SCAN_LIMIT).await {
            Ok(rows) => {
                report.found = rows.len();
                for row in rows {
                    if self.abandon(&row, cutoff, now).await {
                        report.reaped += 1;
                    }
                }
            }
            Err(err) => warn!(error = %err, "the upload reaper could not list stale uploads"),
        }
        self.metrics
            .upload_reaper_scan_duration_seconds(started.elapsed().as_secs_f64());
        report
    }

    /// Fails one row in its own transaction; `true` when this scan did it.
    async fn abandon(
        &self,
        row: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> bool {
        let cleanup = cleanup_payload(row, now);
        match self.repo.abandon(row, cutoff, now, cleanup.as_ref()).await {
            Ok(true) => {
                self.record_abandoned(row);
                true
            }
            Ok(false) => {
                debug!(attachment_id = %row.id, "the upload is no longer abandoned");
                false
            }
            Err(err) => {
                warn!(attachment_id = %row.id, error = %err, "the abandoned upload could not be failed; the next scan retries");
                false
            }
        }
    }

    /// The metrics and the log of a row that was failed.
    fn record_abandoned(&self, row: &StaleUpload) {
        self.metrics.attachment_upload_abandoned(&row.status);
        if row.status == STATUS_PENDING {
            self.metrics.attachments_pending(-1);
        }
        if row.secondary_file_id.is_some() {
            warn!(attachment_id = %row.id, "the abandoned upload has a secondary copy that is not deleted");
        }
    }
}

/// The cleanup message of a reaped row that has a provider file; it carries no secondary
/// reference.
fn cleanup_payload(row: &StaleUpload, now: OffsetDateTime) -> Option<AttachmentCleanupPayload> {
    let file_id = row.provider_file_id.clone()?;
    Some(AttachmentCleanupPayload {
        event_type: AttachmentCleanupEvent::UploadAbandoned,
        tenant_id: row.tenant_id,
        chat_id: row.chat_id,
        attachment_id: row.id,
        provider_file_id: Some(file_id),
        vector_store_id: None,
        storage_backend: row.storage_backend.clone(),
        attachment_kind: row.attachment_kind.clone(),
        deleted_at: now,
        secondary_ref: None,
    })
}
