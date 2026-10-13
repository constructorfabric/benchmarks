//! Upload reaper (DESIGN B.9.5): fails attachments left in `pending` / `uploaded` by an upload
//! that never recorded its outcome and schedules the delete of their provider file.

use std::sync::Arc;
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;

use super::leader::ROLE_UPLOAD_REAPER;
use super::{LeaderElector, SCAN_BATCH, ScanGuard, run_scans};
use crate::api::state::AppServices;
use crate::domain::error::DomainError;
use crate::infra::db::AttachmentStatus;
use crate::infra::db::entity::attachments;
use crate::infra::db::repo::attachments as repo;
use crate::infra::db::tx::write_tx_with_wakes;
use crate::infra::outbox::{AttachmentCleanupEvent, OutboxEnqueuer, OutboxRecord};
use crate::metrics::Metrics;

/// `event_type` of the cleanup of the provider file of an abandoned upload.
const EVENT_UPLOAD_ABANDONED: &str = "attachment_upload_abandoned";

/// The upload reaper; see the module docs.
pub struct UploadReaper {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
    metrics: Arc<Metrics>,
    stale_after: Duration,
}

impl UploadReaper {
    #[must_use]
    pub fn new(services: &AppServices) -> Self {
        Self {
            db: Arc::clone(&services.db),
            outbox: Arc::clone(&services.outbox),
            metrics: Arc::clone(&services.metrics),
            stale_after: Duration::from_secs(services.cfg.upload_reaper.stale_after_secs),
        }
    }

    /// One scan at application time `now`; returns the number of uploads failed.
    ///
    /// A row that fails (for example a database error) is logged and left for the next scan; the
    /// others are still handled.
    ///
    /// # Errors
    /// `Internal` when the candidates cannot be read.
    pub async fn scan_once(&self, now: OffsetDateTime) -> Result<u32, DomainError> {
        self.scan(now, &ScanGuard::unrestricted()).await
    }

    /// [`Self::scan_once`] that stops between rows when `guard` says so.
    async fn scan(&self, now: OffsetDateTime, guard: &ScanGuard) -> Result<u32, DomainError> {
        let started = Instant::now();
        let cutoff = now - self.stale_after;
        let rows = {
            let conn = self.db.conn()?;
            repo::stale_uploads(&conn, &AccessScope::allow_all(), cutoff, SCAN_BATCH).await?
        };
        let mut abandoned = 0;
        for row in &rows {
            if !guard.proceed().await {
                break;
            }
            abandoned += u32::from(self.abandon_logged(row, cutoff, now).await);
        }
        self.metrics
            .upload_reaper_scan_duration_seconds
            .record(started.elapsed().as_secs_f64(), &[]);
        if abandoned > 0 {
            tracing::info!(abandoned, "abandoned uploads failed");
        }
        Ok(abandoned)
    }

    /// [`Self::abandon`] with a failure logged. `true` when the row was failed.
    async fn abandon_logged(
        &self,
        row: &attachments::Model,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> bool {
        match self.abandon(row, cutoff, now).await {
            Ok(failed) => failed,
            Err(err) => {
                tracing::warn!(attachment_id = %row.id, error = %err, "abandoned upload cleanup failed");
                false
            }
        }
    }

    /// Scans every `interval` while `elector` says this process leads the reaper role, until
    /// `cancel`.
    pub async fn run(
        self,
        elector: Arc<dyn LeaderElector>,
        interval: Duration,
        cancel: CancellationToken,
    ) {
        let reaper = &self;
        run_scans(
            ROLE_UPLOAD_REAPER,
            elector,
            interval,
            cancel,
            |guard| async move {
                if let Err(err) = reaper.scan(OffsetDateTime::now_utc(), &guard).await {
                    tracing::warn!(error = %err, "upload reaper scan failed");
                }
            },
        )
        .await;
    }

    /// Fails one stale upload; with a provider file the same transaction schedules its delete.
    /// `false` when the row changed since the scan.
    async fn abandon(
        &self,
        row: &attachments::Model,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let Some(from) = AttachmentStatus::parse(&row.status) else {
            tracing::error!(attachment_id = %row.id, status = %row.status, "unknown attachment status");
            return Ok(false);
        };
        if row.secondary_file_id.is_some() {
            tracing::warn!(attachment_id = %row.id,
                "abandoned upload with a secondary file: the secondary copy is not deleted");
        }
        let event = row
            .provider_file_id
            .is_some()
            .then(|| AttachmentCleanupEvent {
                event_type: EVENT_UPLOAD_ABANDONED.to_owned(),
                tenant_id: row.tenant_id,
                chat_id: row.chat_id,
                attachment_id: row.id,
                provider_file_id: row.provider_file_id.clone(),
                vector_store_id: None,
                storage_backend: row.storage_backend.clone(),
                attachment_kind: row.attachment_kind.clone(),
                deleted_at: now,
                secondary_ref: None,
            });
        let record = event
            .as_ref()
            .map(OutboxRecord::attachment_cleanup)
            .transpose()?;
        let (id, outbox) = (row.id, Arc::clone(&self.outbox));
        let scope = AccessScope::allow_all();

        // Runs again from the CAS on a retried attempt: no effects outside the transaction.
        let failed = write_tx_with_wakes(&self.db, |tx, wakes| {
            let (outbox, scope, record) = (Arc::clone(&outbox), scope.clone(), record.clone());
            Box::pin(async move {
                let schedule = record.is_some();
                if !repo::abandon_stale(tx, &scope, id, from, cutoff, schedule, now).await? {
                    return Ok(false);
                }
                if let Some(record) = record {
                    wakes.add(outbox.enqueue(tx, record).await?);
                }
                Ok(true)
            })
        })
        .await?;

        if failed {
            self.metrics
                .attachment_upload_abandoned
                .add(1, &[KeyValue::new("from_status", from.as_str())]);
        }
        Ok(failed)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration as StdDuration;

    use time::Duration;
    use uuid::Uuid;

    use super::*;
    use crate::infra::db::ts::db_now;
    use crate::test_support::app::{TestApp, ctx};
    use crate::test_support::attachments::attachment_row;
    use crate::test_support::stream::create_chat;
    use crate::test_support::workers::{SeedUpload, seed_upload};

    const CLEANUP_QUEUE: &str = "mini-chat.attachment_cleanup";

    #[tokio::test]
    async fn reaper_fails_seeded_stale_upload() {
        let app = TestApp::builder().quiet_cleanup().build().await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &ctx(tenant, user), None).await;
        let stale = db_now() - Duration::minutes(10);
        let seed = |status, file: Option<&str>, at, cleanup| SeedUpload {
            tenant,
            chat,
            uploader: user,
            status,
            provider_file_id: file.map(str::to_owned),
            updated_at: at,
            cleanup_status: cleanup,
        };
        let pending = seed_upload(&app.db, &seed("pending", None, stale, None)).await;
        let uploaded = seed_upload(&app.db, &seed("uploaded", Some("file-abc"), stale, None)).await;
        let owned = seed_upload(
            &app.db,
            &seed("uploaded", Some("file-own"), stale, Some("pending")),
        )
        .await;
        let fresh = seed_upload(
            &app.db,
            &seed("pending", None, db_now() - Duration::seconds(10), None),
        )
        .await;
        let reaper = UploadReaper::new(&app.services);
        let now = db_now();

        assert_eq!(reaper.scan_once(now).await.unwrap(), 2);

        let pending = attachment_row(&app, chat, pending).await;
        assert_eq!(
            (pending.status.as_str(), pending.error_code.as_deref()),
            ("failed", Some("upload_abandoned"))
        );
        assert!(
            pending.cleanup_status.is_none(),
            "no file, nothing to clean"
        );
        assert!(pending.updated_at > stale);
        assert!(pending.deleted_at.is_none());

        let uploaded_row = attachment_row(&app, chat, uploaded).await;
        assert_eq!(
            (
                uploaded_row.status.as_str(),
                uploaded_row.error_code.as_deref()
            ),
            ("failed", Some("upload_abandoned"))
        );
        assert_eq!(uploaded_row.cleanup_status.as_deref(), Some("pending"));
        assert!(uploaded_row.cleanup_updated_at.is_some());
        TestApp::wait_until("cleanup event delivered", || async {
            !app.outbox_payloads(CLEANUP_QUEUE).is_empty()
        })
        .await;
        let payloads = app.outbox_payloads(CLEANUP_QUEUE);
        assert_eq!(payloads.len(), 1, "{payloads:?}");
        assert_eq!(payloads[0]["event_type"], "attachment_upload_abandoned");
        assert_eq!(payloads[0]["attachment_id"], uploaded.to_string());
        assert_eq!(payloads[0]["provider_file_id"], "file-abc");
        assert_eq!(payloads[0]["storage_backend"], "openai");
        assert!(payloads[0]["secondary_ref"].is_null());

        let owned = attachment_row(&app, chat, owned).await;
        assert_eq!(owned.status, "uploaded", "owned by the chat cleanup");
        assert!(owned.error_code.is_none());
        let fresh = attachment_row(&app, chat, fresh).await;
        assert_eq!(fresh.status, "pending");

        assert_eq!(reaper.scan_once(db_now()).await.unwrap(), 0);
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        assert_eq!(app.outbox_payloads(CLEANUP_QUEUE).len(), 1);
    }
}
