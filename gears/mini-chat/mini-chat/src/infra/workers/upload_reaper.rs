//! Upload reaper worker (DESIGN B.9.5, section 3.2 "upload reaper").
//!
//! Every `upload_reaper.scan_interval_secs`, while this process leads the
//! `upload-reaper` role, one scan selects at most 100 attachments with
//! `status IN (pending, uploaded)`, `deleted_at IS NULL`, `cleanup_status IS
//! NULL` and `updated_at < now - stale_after_secs` (application clock),
//! oldest `updated_at` first. Each row gets its own transaction
//! ([`attachment_repo::abandon_upload_in_tx`]): the CAS to `failed` /
//! `upload_abandoned` guarded by the same predicates (the first statement)
//! and, when the row has a `provider_file_id`, `cleanup_status = pending`
//! plus an `attachment_upload_abandoned` cleanup message (without
//! `secondary_ref`: a secondary copy is logged and left in place). A CAS
//! that matches nothing (the upload finished, the row was deleted or
//! claimed by chat cleanup) skips the row.
//!
//! Metrics: `{prefix}_attachment_upload_abandoned{from_status}` (after the
//! commit) and `{prefix}_upload_reaper_scan_duration` (seconds).

use std::sync::Arc;
use std::time::Instant;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::outbox::in_transaction;
use tracing::{info, warn};

use super::{LeaderElector, SCAN_LIMIT, WorkerRole, meter, run_periodic};
use crate::config::UploadReaperConfig;
use crate::domain::error::DomainError;
use crate::domain::ports::storage_now;
use crate::infra::db::repo::DbRepository;
use crate::infra::db::repo::attachments::{self as attachment_repo, AbandonedUpload, StaleUpload};

/// Outcome counts of one scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReaperScanReport {
    /// Rows the scan returned.
    pub selected: usize,
    /// Rows failed as `upload_abandoned`.
    pub abandoned: usize,
    /// ... of which an attachment cleanup message was enqueued for.
    pub cleanup_enqueued: usize,
    /// Rows whose CAS matched nothing.
    pub skipped: usize,
    /// Rows whose transaction failed.
    pub failed: usize,
    /// The scan stopped early (cancellation or lost leadership).
    pub interrupted: bool,
}

struct ReaperMetrics {
    abandoned: Counter<u64>,
    scan_duration: Histogram<f64>,
}

impl ReaperMetrics {
    fn new(prefix: &str) -> Self {
        let meter = meter();
        Self {
            abandoned: meter
                .u64_counter(format!("{prefix}_attachment_upload_abandoned"))
                .with_description("Uploads failed as abandoned by the upload reaper")
                .build(),
            scan_duration: meter
                .f64_histogram(format!("{prefix}_upload_reaper_scan_duration"))
                .with_unit("s")
                .with_description("Duration of one upload reaper scan")
                .build(),
        }
    }
}

/// The upload reaper (module docs).
pub struct UploadReaper {
    repo: Arc<DbRepository>,
    elector: Arc<dyn LeaderElector>,
    config: UploadReaperConfig,
    metrics: ReaperMetrics,
}

impl std::fmt::Debug for UploadReaper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadReaper")
            .field("config", &self.config)
            .field("elector", &self.elector)
            .finish_non_exhaustive()
    }
}

impl UploadReaper {
    /// Reaper over the repositories (and their outbox producer), the elector
    /// of [`WorkerRole::UploadReaper`] and the `upload_reaper` section;
    /// metrics use `metrics_prefix`.
    #[must_use]
    pub fn new(
        repo: Arc<DbRepository>,
        elector: Arc<dyn LeaderElector>,
        config: &UploadReaperConfig,
        metrics_prefix: &str,
    ) -> Self {
        Self {
            repo,
            elector,
            config: config.clone(),
            metrics: ReaperMetrics::new(metrics_prefix),
        }
    }

    /// Scans every `scan_interval_secs` until `cancel` fires; returns at once
    /// when the reaper is disabled.
    pub async fn run(&self, cancel: &CancellationToken) {
        if !self.config.enabled {
            info!(target: "mini_chat.upload_reaper", "upload reaper disabled");
            return;
        }
        info!(
            target: "mini_chat.upload_reaper",
            scan_interval_secs = self.config.scan_interval_secs,
            stale_after_secs = self.config.stale_after_secs,
            "upload reaper started"
        );
        run_periodic(
            WorkerRole::UploadReaper,
            self.config.scan_interval_secs,
            self.elector.as_ref(),
            cancel,
            || async {
                self.scan(cancel).await;
            },
        )
        .await;
    }

    /// The scan cutoff at `now`: `now - stale_after_secs`.
    #[must_use]
    pub fn cutoff(&self, now: OffsetDateTime) -> OffsetDateTime {
        let stale = i64::try_from(self.config.stale_after_secs).unwrap_or(i64::MAX);
        now.saturating_sub(time::Duration::seconds(stale))
    }

    /// One scan at the current time: abandons every selected row until
    /// `cancel` fires or leadership is lost.
    pub async fn scan(&self, cancel: &CancellationToken) -> ReaperScanReport {
        let started = Instant::now();
        let report = self.scan_rows(cancel).await;
        self.metrics
            .scan_duration
            .record(started.elapsed().as_secs_f64(), &[]);
        if report.abandoned > 0 || report.failed > 0 {
            info!(
                target: "mini_chat.upload_reaper",
                selected = report.selected,
                abandoned = report.abandoned,
                cleanup_enqueued = report.cleanup_enqueued,
                skipped = report.skipped,
                failed = report.failed,
                "upload reaper scan done"
            );
        }
        report
    }

    async fn scan_rows(&self, cancel: &CancellationToken) -> ReaperScanReport {
        let mut report = ReaperScanReport::default();
        let cutoff = self.cutoff(storage_now());
        let rows = match self.select(cutoff).await {
            Ok(rows) => rows,
            Err(err) => {
                warn!(target: "mini_chat.upload_reaper", error = %err, "upload reaper scan failed");
                return report;
            }
        };
        report.selected = rows.len();
        for upload in rows {
            if cancel.is_cancelled() || !self.elector.is_leader() {
                report.interrupted = true;
                break;
            }
            self.process_row(upload, cutoff, &mut report).await;
        }
        report
    }

    /// Abandons one selected row and counts the outcome in `report`.
    async fn process_row(
        &self,
        upload: StaleUpload,
        cutoff: OffsetDateTime,
        report: &mut ReaperScanReport,
    ) {
        match self.abandon(upload, cutoff).await {
            Ok(Some(abandoned)) => {
                self.record_abandoned(upload, &abandoned);
                report.abandoned += 1;
                report.cleanup_enqueued += usize::from(abandoned.cleanup_enqueued);
            }
            Ok(None) => report.skipped += 1,
            Err(err) => {
                report.failed += 1;
                warn!(
                    target: "mini_chat.upload_reaper",
                    attachment_id = %upload.id,
                    error = %err,
                    "abandoning the upload failed; retried by a later scan"
                );
            }
        }
    }

    async fn select(&self, cutoff: OffsetDateTime) -> Result<Vec<StaleUpload>, DomainError> {
        let conn = self.repo.db().conn()?;
        attachment_repo::stale_uploads(&conn, self.repo.timestamps(), cutoff, SCAN_LIMIT).await
    }

    async fn abandon(
        &self,
        upload: StaleUpload,
        cutoff: OffsetDateTime,
    ) -> Result<Option<AbandonedUpload>, DomainError> {
        let db = self.repo.db().db();
        let ts = self.repo.timestamps();
        let outbox = Arc::clone(self.repo.outbox());
        let at = storage_now();
        in_transaction(&db, move |tx| {
            Box::pin(async move {
                attachment_repo::abandon_upload_in_tx(tx, ts, &outbox, upload, cutoff, at).await
            })
        })
        .await
    }

    fn record_abandoned(&self, upload: StaleUpload, abandoned: &AbandonedUpload) {
        self.metrics.abandoned.add(
            1,
            &[KeyValue::new("from_status", abandoned.from_status.as_str())],
        );
        info!(
            target: "mini_chat.upload_reaper",
            attachment_id = %upload.id,
            chat_id = %upload.chat_id,
            from_status = abandoned.from_status.as_str(),
            cleanup_enqueued = abandoned.cleanup_enqueued,
            "abandoned upload failed"
        );
        if let Some(secondary_file_id) = &abandoned.secondary_file_id {
            warn!(
                target: "mini_chat.upload_reaper",
                attachment_id = %upload.id,
                secondary_file_id = %secondary_file_id,
                "abandoned upload has a secondary copy the cleanup does not delete"
            );
        }
    }
}

#[cfg(test)]
#[path = "upload_reaper_tests.rs"]
mod tests;
