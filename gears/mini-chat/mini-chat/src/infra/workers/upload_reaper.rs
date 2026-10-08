//! Upload reaper (B.9.5, S§10.4).
//!
//! Each scan (leader only) reads at most [`MAX_CANDIDATES`] uploads left
//! `pending` / `uploaded` (not deleted, no `cleanup_status`) whose
//! `updated_at` is older than `now - upload_reaper.stale_after_secs`,
//! oldest first. Per row one transaction: a CAS (same predicate) to
//! `failed` / `upload_abandoned`; a row with a provider file also gets
//! `cleanup_status = 'pending'` and an `attachment_upload_abandoned`
//! cleanup event. Rows claimed by chat cleanup (`cleanup_status` set) are
//! never touched. The row is not soft-deleted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;
use tracing::{info, warn};

use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::ports::{OutboxPort, PendingWakes};
use crate::domain::services::AppServices;
use crate::domain::services::attachment_service::cleanup_payload;
use crate::infra::db::entity::attachment;
use crate::infra::db::repos::AttachmentRepo;
use crate::infra::db::timestamps::comparable;
use crate::infra::db::tx::with_retry;
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::outbox::payloads::AttachmentCleanupEventType;
use crate::infra::workers::leader::{LeaderElector, UPLOAD_REAPER_ROLE};
use crate::infra::workers::run_periodic;

/// Rows read per scan (fixed, B.9.5).
pub const MAX_CANDIDATES: u64 = 100;

/// Result of one scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Stale rows found.
    pub candidates: u64,
    /// Rows moved to `failed` / `upload_abandoned`.
    pub abandoned: u64,
    /// Abandoned rows with a provider file (cleanup event enqueued).
    pub cleanup_enqueued: u64,
}

/// What one row's transaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reaped {
    /// The CAS updated no row (finished, deleted or claimed meanwhile).
    Skipped,
    Abandoned {
        cleanup: bool,
    },
}

/// Fails abandoned uploads.
pub struct UploadReaper {
    db: Arc<DBProvider<DomainError>>,
    clock: Arc<dyn Clock>,
    outbox: Arc<dyn OutboxPort>,
    metrics: Arc<MiniChatMetrics>,
    stale_after: time::Duration,
    interval: Duration,
}

impl UploadReaper {
    /// Reaper over the services' database and outbox, with the
    /// `upload_reaper` configuration.
    #[must_use]
    pub fn new(s: &AppServices) -> Self {
        let cfg = &s.config.upload_reaper;
        Self {
            db: Arc::clone(&s.db),
            clock: Arc::clone(&s.clock),
            outbox: Arc::clone(&s.outbox),
            metrics: Arc::clone(&s.metrics),
            stale_after: time::Duration::seconds(
                i64::try_from(cfg.stale_after_secs).unwrap_or(i64::MAX),
            ),
            interval: Duration::from_secs(cfg.scan_interval_secs),
        }
    }

    /// Scan every `upload_reaper.scan_interval_secs` while leader, until
    /// `cancel`.
    #[must_use = "the task is detached when the handle is dropped"]
    pub fn spawn(
        self: Arc<Self>,
        elector: Arc<dyn LeaderElector>,
        cancel: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let interval = self.interval;
            run_periodic(UPLOAD_REAPER_ROLE, interval, elector, cancel, || {
                let me = Arc::clone(&self);
                async move {
                    me.scan_once(me.clock.now()).await;
                }
            })
            .await;
        })
    }

    /// One scan at `now`. Failures are logged; the affected rows are
    /// retried by a later scan.
    pub async fn scan_once(&self, now: OffsetDateTime) -> ReapReport {
        let started = Instant::now();
        let mut report = ReapReport::default();
        let rows = match self.candidates(now).await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "upload reaper scan failed");
                self.metrics.upload_reaper_scan_duration(started.elapsed());
                return report;
            }
        };
        for row in &rows {
            report.candidates += 1;
            match self.try_reap(row, now).await {
                Reaped::Abandoned { cleanup } => {
                    report.abandoned += 1;
                    report.cleanup_enqueued += u64::from(cleanup);
                }
                Reaped::Skipped => {}
            }
        }
        self.metrics.upload_reaper_scan_duration(started.elapsed());
        report
    }

    /// [`Self::reap`] with its outcome logged and recorded (a failure
    /// counts as skipped).
    async fn try_reap(&self, row: &attachment::Model, now: OffsetDateTime) -> Reaped {
        match self.reap(row, now).await {
            Ok(reaped @ Reaped::Abandoned { cleanup }) => {
                self.metrics.upload_abandoned(from_status(row));
                info!(attachment_id = %row.id, from_status = %row.status, cleanup, "upload abandoned");
                reaped
            }
            Ok(Reaped::Skipped) => Reaped::Skipped,
            Err(e) => {
                warn!(attachment_id = %row.id, error = %e, "upload reaper failed for a row; retried by a later scan");
                Reaped::Skipped
            }
        }
    }

    /// Stale rows of every tenant at `now`.
    async fn candidates(&self, now: OffsetDateTime) -> Result<Vec<attachment::Model>, DomainError> {
        let conn = self.db.conn()?;
        let cutoff = comparable(self.db.db().backend(), now - self.stale_after);
        Ok(AttachmentRepo
            .list_abandoned(&conn, &AccessScope::allow_all(), &cutoff, MAX_CANDIDATES)
            .await?)
    }

    /// CAS + optional cleanup event of one row, one transaction.
    async fn reap(
        &self,
        row: &attachment::Model,
        now: OffsetDateTime,
    ) -> Result<Reaped, DomainError> {
        let cutoff = comparable(self.db.db().backend(), now - self.stale_after);
        let scope = AccessScope::for_tenant(row.tenant_id);
        let cleanup = row.provider_file_id.is_some();
        let payload = cleanup_payload(
            row,
            AttachmentCleanupEventType::AttachmentUploadAbandoned,
            now,
        );
        let (chat_id, id, status) = (row.chat_id, row.id, row.status.clone());
        let outbox = Arc::clone(&self.outbox);
        let (reaped, wakes) = with_retry(&self.db, move |tx| {
            let (outbox, scope, cutoff, payload, status) = (
                Arc::clone(&outbox),
                scope.clone(),
                cutoff.clone(),
                payload.clone(),
                status.clone(),
            );
            Box::pin(async move {
                let mut wakes = PendingWakes::new();
                let n = AttachmentRepo
                    .mark_abandoned(tx, &scope, chat_id, id, &status, &cutoff, cleanup, now)
                    .await?;
                if n == 0 {
                    return Ok((Reaped::Skipped, wakes));
                }
                if cleanup {
                    outbox
                        .enqueue_attachment_cleanup(tx, &payload, &mut wakes)
                        .await?;
                }
                Ok((Reaped::Abandoned { cleanup }, wakes))
            })
        })
        .await?;
        wakes.fire_all();
        if let (Reaped::Abandoned { .. }, Some(secondary)) = (&reaped, &row.secondary_file_id) {
            // The cleanup message carries no secondary_ref (D "Attachment Deletion").
            warn!(attachment_id = %id, secondary_file_id = %secondary, "abandoned upload: its secondary (Anthropic) copy is not deleted");
        }
        Ok(reaped)
    }
}

/// Metric label of the row's status before the reaper.
fn from_status(row: &attachment::Model) -> &'static str {
    if row.status == "pending" {
        "pending"
    } else {
        "uploaded"
    }
}
