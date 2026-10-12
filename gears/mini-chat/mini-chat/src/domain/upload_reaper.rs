//! Upload reaper (DESIGN B.9.5, 3.3 "Upload Attachment"): a periodic scan fails
//! the uploads whose request or background task died.
//!
//! An attachment is abandoned when it is `pending` or `uploaded`, live, not
//! marked for cleanup and its `updated_at` is older than `now -
//! upload_reaper.stale_after_secs` (application clock, strictly). Each scan
//! takes at most [`SCAN_BATCH`] candidates, oldest first; every candidate is
//! handled in **one** transaction whose first statement is a conditional update
//! (`failed`, `error_code = upload_abandoned`) that re-checks the whole
//! predicate: only the winner (`rows_affected = 1`) goes on, and, when the row
//! has a provider file, marks its cleanup `pending` and enqueues the
//! `attachment_upload_abandoned` message in the same transaction. A failing
//! enqueue rolls that row back and the scan continues with the others.

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::TxConfig;
use toolkit_security::AccessScope;

use crate::config::MiniChatConfig;
use crate::domain::attachments::{AttachmentCleanupEvent, AttachmentCleanupKind, AttachmentRow};
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::ports::{AttachmentCleanupPort, AttachmentCleanupRepository};

/// Candidates taken by one scan (fixed, not configurable).
pub const SCAN_BATCH: u64 = 100;

/// Fails abandoned uploads.
pub struct UploadReaper {
    db: Arc<DBProvider<DomainError>>,
    config: Arc<MiniChatConfig>,
    cleanup: Option<Arc<dyn AttachmentCleanupPort>>,
}

impl UploadReaper {
    /// Creates the reaper. Without [`Self::with_cleanup`] a reaped row gets no
    /// provider cleanup.
    #[must_use]
    pub fn new(db: Arc<DBProvider<DomainError>>, config: Arc<MiniChatConfig>) -> Self {
        Self {
            db,
            config,
            cleanup: None,
        }
    }

    /// Wires the attachment-cleanup outbox port.
    #[must_use]
    pub fn with_cleanup<C: AttachmentCleanupPort + 'static>(mut self, cleanup: Arc<C>) -> Self {
        self.cleanup = Some(cleanup);
        self
    }

    /// The periodic task: returns at once when `upload_reaper.enabled` is false,
    /// otherwise scans every `scan_interval_secs` (the first scan right at the
    /// start) with the application clock until `cancel` is cancelled. A failing
    /// scan is logged and the loop goes on.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let settings = &self.config.upload_reaper;
        if !settings.enabled {
            tracing::info!("upload reaper is disabled");
            return;
        }
        let mut ticker = tokio::time::interval(Duration::from_secs(settings.scan_interval_secs));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                _ = ticker.tick() => {}
            }
            self.scan().await;
        }
    }

    /// One periodic scan; a failure is logged.
    async fn scan(&self) {
        match self.sweep_at(now_utc()).await {
            Ok(0) => {}
            Ok(reaped) => tracing::info!(reaped, "upload reaper failed abandoned uploads"),
            Err(error) => tracing::warn!(%error, "upload reaper scan failed"),
        }
    }

    /// One scan at the application time `now`; returns the number of rows this
    /// call failed (conditional-update wins). A row that cannot be reaped now
    /// (its enqueue failed) is logged and left for the next scan.
    ///
    /// # Errors
    /// `Internal` when the candidates cannot be read.
    pub async fn sweep_at(&self, now: OffsetDateTime) -> Result<usize, DomainError> {
        let stale = i64::try_from(self.config.upload_reaper.stale_after_secs).unwrap_or(i64::MAX);
        let Some(cutoff) = now.checked_sub(time::Duration::seconds(stale)) else {
            return Ok(0);
        };
        // The scan crosses tenants: it is a system task, not a request. Each
        // candidate is then handled with the scope of its own tenant.
        let candidates = self
            .db
            .conn()?
            .stale_attachments(&AccessScope::allow_all(), cutoff, SCAN_BATCH)
            .await?;
        let mut reaped = 0;
        for candidate in candidates {
            match self.reap(&candidate, cutoff, now).await {
                Ok(true) => reaped += 1,
                Ok(false) => {
                    tracing::debug!(attachment_id = %candidate.id,
                        "abandoned upload candidate is no longer reapable");
                }
                Err(error) => {
                    tracing::warn!(attachment_id = %candidate.id, %error,
                        "abandoned upload was not reaped; the next scan retries it");
                }
            }
        }
        Ok(reaped)
    }

    async fn reap(
        &self,
        candidate: &AttachmentRow,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        let scope = AccessScope::for_tenant(candidate.tenant_id);
        let cleanup = self
            .cleanup
            .clone()
            .filter(|_| candidate.provider_file_id.is_some());
        if let Some(secondary) = &candidate.secondary_file_id {
            tracing::warn!(attachment_id = %candidate.id, secondary_file_id = %secondary,
                "reaped upload has a secondary provider copy that is not deleted");
        }
        let event =
            AttachmentCleanupEvent::for_row(AttachmentCleanupKind::UploadAbandoned, candidate, now);
        let wake = self
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let (scope, candidate, cleanup, event) = (
                    scope.clone(),
                    candidate.clone(),
                    cleanup.clone(),
                    event.clone(),
                );
                Box::pin(async move {
                    let owed = cleanup.is_some();
                    if !tx
                        .reap_attachment(&scope, &candidate, cutoff, owed, now)
                        .await?
                    {
                        return Ok(None);
                    }
                    match cleanup {
                        Some(cleanup) => cleanup
                            .enqueue_attachment_cleanup(tx, &event)
                            .await
                            .map(Some),
                        None => Ok(Some(toolkit_db::outbox::Wake::empty())),
                    }
                })
            })
            .await?;
        Ok(match wake {
            Some(wake) => {
                wake.fire();
                true
            }
            None => false,
        })
    }
}
