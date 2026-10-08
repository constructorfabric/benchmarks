//! Upload reaper (DESIGN B.9.5).
//!
//! Each scan (leader only) reads at most [`SCAN_BATCH`] live `pending` /
//! `uploaded` rows without a cleanup state whose `updated_at` is before `now -
//! stale_after_secs`, oldest first. Per row, one transaction (Ruling R5:
//! write first): the guarded update to `failed` / `upload_abandoned` (plus
//! `cleanup_status = 'pending'` when the row has a provider file), then the
//! `attachment_upload_abandoned` cleanup event for that file. The row is not
//! soft-deleted. Metrics are recorded as log events.

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use toolkit_db::DBProvider;

use toolkit_db::outbox::Wake;

use super::{LeaderElector, ROLE_UPLOAD_REAPER, SCAN_BATCH};
use crate::domain::error::DomainError;
use crate::domain::time::{db_now, db_ts};
use crate::infra::db::entities::attachment;
use crate::infra::db::repos::attachment_repo::{self, AbandonUpload};
use crate::infra::outbox::MiniChatOutbox;
use crate::infra::outbox::payloads::AttachmentCleanupPayload;

/// `event_type` of the cleanup message for an abandoned upload's file.
const EVENT_UPLOAD_ABANDONED: &str = "attachment_upload_abandoned";

pub struct UploadReaper {
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<MiniChatOutbox>,
    elector: Arc<dyn LeaderElector>,
    stale_after: Duration,
}

impl UploadReaper {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        outbox: Arc<MiniChatOutbox>,
        elector: Arc<dyn LeaderElector>,
        stale_after: Duration,
    ) -> Self {
        Self {
            db,
            outbox,
            elector,
            stale_after,
        }
    }

    /// One scan at `now`; returns the number of rows this scan failed. A
    /// non-leader scans nothing, and a scan stops once leadership is lost.
    ///
    /// # Errors
    /// Database failure of the candidate query.
    pub async fn scan_once(&self, now: OffsetDateTime) -> Result<u32, DomainError> {
        if !self.leads() {
            return Ok(0);
        }
        let started = std::time::Instant::now();
        let cutoff = db_ts(now - self.stale_after);
        let rows = {
            let conn = self.db.conn()?;
            attachment_repo::stale_uploads(&conn, cutoff, SCAN_BATCH).await?
        };
        let mut reaped = 0u32;
        for row in &rows {
            if !self.leads() {
                tracing::info!("upload reaper: leadership lost; scan stopped");
                break;
            }
            if self.reap_logged(row, cutoff).await {
                reaped += 1;
            }
        }
        tracing::debug!(
            candidates = rows.len(),
            reaped,
            duration_ms = started.elapsed().as_millis(),
            "upload reaper scan (mini_chat_upload_reaper_scan_duration_seconds)"
        );
        Ok(reaped)
    }

    /// [`Self::reap`] with the outcome logged; failures are retried by a
    /// later scan.
    async fn reap_logged(&self, row: &attachment::Model, cutoff: OffsetDateTime) -> bool {
        let res = self.reap(row, cutoff).await;
        log_outcome(row, &res);
        matches!(res, Ok(true))
    }

    fn leads(&self) -> bool {
        self.elector.is_leader(ROLE_UPLOAD_REAPER)
    }

    /// One row's transaction; `false` when the guarded update matched nothing.
    async fn reap(
        &self,
        row: &attachment::Model,
        cutoff: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let outbox = Arc::clone(&self.outbox);
        let target = AbandonUpload {
            tenant_id: row.tenant_id,
            id: row.id,
            from_status: row.status.clone(),
            cutoff,
            with_cleanup: row.provider_file_id.is_some(),
        };
        let row = row.clone();
        let wake = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    let now = db_now();
                    if attachment_repo::abandon_upload(tx, &target, now).await? == 0 {
                        return Ok(None);
                    }
                    let Some(file_id) = row.provider_file_id else {
                        return Ok(Some(Wake::empty()));
                    };
                    let payload = AttachmentCleanupPayload {
                        event_type: EVENT_UPLOAD_ABANDONED.to_owned(),
                        tenant_id: row.tenant_id,
                        chat_id: row.chat_id,
                        attachment_id: row.id,
                        provider_file_id: Some(file_id),
                        vector_store_id: None,
                        storage_backend: row.storage_backend,
                        attachment_kind: row.attachment_kind,
                        deleted_at: now,
                        secondary_ref: None,
                    };
                    Ok(Some(outbox.enqueue_attachment_cleanup(tx, &payload).await?))
                })
            })
            .await?;
        Ok(wake.map(Wake::fire).is_some())
    }
}

fn log_outcome(row: &attachment::Model, res: &Result<bool, DomainError>) {
    match res {
        Ok(true) => tracing::info!(
            attachment_id = %row.id,
            from_status = %row.status,
            "abandoned upload failed (mini_chat_attachment_upload_abandoned_total)"
        ),
        // the guarded update matched nothing: no longer a candidate
        Ok(false) => {}
        Err(e) => tracing::warn!(
            attachment_id = %row.id,
            error = %e,
            "abandoned upload not recorded; retried by a later scan"
        ),
    }
}
