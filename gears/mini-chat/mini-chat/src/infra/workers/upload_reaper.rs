//! Upload reaper (DESIGN B.9.5, §3.2 "upload reaper", spec §13.2): fails
//! attachments an upload left in `pending` or `uploaded` (dropped request,
//! process stop) once their `updated_at` is older than
//! `upload_reaper.stale_after_secs`.
//!
//! Each scan reads at most 100 candidates (oldest `updated_at` first; advisory
//! only). Per candidate, one transaction: the guarded update to `failed`
//! (`upload_abandoned`) and, when the row has a provider file, `cleanup_status =
//! 'pending'` plus the `attachment_upload_abandoned` cleanup message. A row that
//! changed meanwhile (finished, deleted, claimed by chat cleanup) is skipped.
//! The row is not soft-deleted.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use tracing::{debug, error, info, warn};

use super::leader::{LeaderElector, UPLOAD_REAPER_ROLE};
use crate::config::MiniChatConfig;
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::db::entities::attachment;
use crate::infra::db::repos::AttachmentRepo;
use crate::infra::db::tx::with_tx_retry;
use crate::infra::outbox::payloads::{
    ATTACHMENT_CLEANUP_PAYLOAD_TYPE, AttachmentCleanupEvent, AttachmentCleanupPayload,
};
use crate::infra::outbox::{OutboxEnqueuer, PendingWakes, QueueKind};

/// Candidates per scan (fixed; the rest are picked up by later scans).
const SCAN_LIMIT: u64 = 100;

/// Infrastructure of [`UploadReaper`].
pub struct UploadReaperDeps {
    pub config: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub elector: Arc<dyn LeaderElector>,
}

/// The upload reaper.
pub struct UploadReaper {
    cfg: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
    outbox: Arc<OutboxEnqueuer>,
    elector: Arc<dyn LeaderElector>,
}

impl UploadReaper {
    #[must_use]
    pub fn new(deps: UploadReaperDeps) -> Self {
        let UploadReaperDeps {
            config,
            db,
            outbox,
            elector,
        } = deps;
        Self {
            cfg: config,
            db,
            outbox,
            elector,
        }
    }

    /// Run the scans every `scan_interval_secs` while this instance leads the
    /// `upload-reaper` role, until `cancel` fires.
    #[must_use = "the handle reports when the task ended; drop it to detach"]
    pub fn spawn(self: Arc<Self>, cancel: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(
                self.cfg.upload_reaper.scan_interval_secs,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            info!("upload reaper started");
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                let scan = async {
                    if !self.elector.is_leader(UPLOAD_REAPER_ROLE).await {
                        debug!("upload reaper: not the leader; scan skipped");
                        return;
                    }
                    match self.scan_once(now_utc()).await {
                        Ok(0) => {}
                        Ok(n) => info!(failed = n, "upload reaper failed abandoned uploads"),
                        Err(err) => error!(%err, "upload reaper scan failed"),
                    }
                };
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => break,
                    () = scan => {}
                }
            }
            info!("upload reaper stopped");
        })
    }

    /// One scan at application time `now`; returns the number of rows this scan
    /// failed. A row that cannot be updated (database failure) is logged and
    /// retried by the next scan.
    ///
    /// # Errors
    /// The candidate query failed.
    pub async fn scan_once(&self, now: DateTime<Utc>) -> DomainResult<u32> {
        let stale_after =
            i64::try_from(self.cfg.upload_reaper.stale_after_secs).unwrap_or(i64::MAX);
        let cutoff = now - chrono::Duration::seconds(stale_after);
        let candidates = {
            let conn = self.db.conn()?;
            AttachmentRepo::stale_uploads(&conn, cutoff, SCAN_LIMIT).await?
        };
        let mut failed = 0_u32;
        for row in candidates {
            match self.reap(&row, cutoff, now).await {
                Ok(true) => {
                    // mini_chat_attachment_upload_abandoned_total{from_status}
                    failed += 1;
                }
                Ok(false) => {
                    debug!(attachment_id = %row.id, "abandoned upload changed meanwhile; skipped");
                }
                Err(err) => {
                    error!(attachment_id = %row.id, %err, "cannot fail the abandoned upload; retrying next scan");
                }
            }
        }
        Ok(failed)
    }

    /// Fail one candidate; `false` when the guarded update matched no row.
    async fn reap(
        &self,
        row: &attachment::Model,
        cutoff: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> DomainResult<bool> {
        let outbox = Arc::clone(&self.outbox);
        let row = row.clone();
        let wakes: Option<PendingWakes> =
            with_tx_retry(&self.db, "abandoned upload reap", move |tx| {
                let (row, outbox) = (row.clone(), Arc::clone(&outbox));
                Box::pin(async move {
                    let has_file = row.provider_file_id.is_some();
                    if !AttachmentRepo::fail_abandoned(
                        tx,
                        row.tenant_id,
                        row.id,
                        &row.status,
                        cutoff,
                        now,
                        has_file,
                    )
                    .await?
                    {
                        return Ok::<_, DomainError>(None);
                    }
                    let tenant_id = row.tenant_id;
                    if let Some(secondary) = &row.secondary_file_id {
                        // The cleanup message carries no `secondary_ref`.
                        warn!(
                            attachment_id = %row.id,
                            secondary_file_id = %secondary,
                            "abandoned upload has a secondary provider copy that is not deleted"
                        );
                    }
                    let mut wakes = PendingWakes::new();
                    let Some(provider_file_id) = row.provider_file_id else {
                        return Ok(Some(wakes));
                    };
                    let payload = AttachmentCleanupPayload {
                        event_type: AttachmentCleanupEvent::AttachmentUploadAbandoned,
                        tenant_id,
                        chat_id: row.chat_id,
                        attachment_id: row.id,
                        provider_file_id: Some(provider_file_id),
                        vector_store_id: None,
                        storage_backend: row.storage_backend,
                        attachment_kind: row.attachment_kind,
                        deleted_at: now,
                        secondary_ref: None,
                    };
                    wakes.push(
                        outbox
                            .enqueue_json(
                                tx,
                                QueueKind::AttachmentCleanup,
                                tenant_id,
                                ATTACHMENT_CLEANUP_PAYLOAD_TYPE,
                                &payload,
                            )
                            .await?,
                    );
                    Ok(Some(wakes))
                })
            })
            .await?;
        match wakes {
            None => Ok(false),
            Some(wakes) => {
                wakes.fire();
                Ok(true)
            }
        }
    }
}
