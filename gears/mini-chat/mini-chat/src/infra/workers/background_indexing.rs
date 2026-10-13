//! Background indexing (DESIGN §3.6 "File Upload", B.9.5, spec §13.3): keeps
//! polling the vector store for a document that was still `in_progress` at
//! the upload deadline.
//!
//! Rounds of `background_round` (20 s) up to `background_limit` (10 min); each
//! round first refreshes `updated_at` (guarded by `status = 'uploaded' AND
//! cleanup_status IS NULL AND deleted_at IS NULL`, so the upload reaper leaves
//! the row alone) and then reads the status with waits from `poll_initial`
//! doubling to `background_poll_max`. `completed` → `ready` (4 attempts,
//! `set_ready_retry` doubling); `failed` / `cancelled` / a non-transient read
//! error / the limit → one transaction: `failed` (`indexing_failed`),
//! `cleanup_status = 'pending'` and the `attachment_indexing_failed` cleanup
//! message. A row that is deleted, no longer `uploaded` or handed to cleanup
//! stops the task without changes; so does the gear-stop token.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::services::attachment::UploadTimings;
use crate::infra::db::repos::AttachmentRepo;
use crate::infra::db::tx::with_tx_retry;
use crate::infra::llm::{RagClient, ResolvedProvider, VsFileStatus};
use crate::infra::outbox::payloads::{
    ATTACHMENT_CLEANUP_PAYLOAD_TYPE, AttachmentCleanupEvent, AttachmentCleanupPayload,
};
use crate::infra::outbox::{OutboxEnqueuer, QueueKind};

/// Number of `ready` writes before giving up (1 + 3 retries).
const SET_READY_ATTEMPTS: u32 = 4;

/// Shared infrastructure of the background indexing tasks.
#[derive(Clone)]
pub struct IndexingDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub rag: Arc<RagClient>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub timings: UploadTimings,
    /// Gear-stop token.
    pub stop: CancellationToken,
}

/// The document whose indexing continues in the background.
#[derive(Debug, Clone)]
pub struct IndexingJob {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider: ResolvedProvider,
    pub vector_store_id: String,
    pub provider_file_id: String,
    pub storage_backend: String,
    pub attachment_kind: String,
}

/// How a background indexing task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexingOutcome {
    /// The attachment is `ready`.
    Ready,
    /// The attachment is `failed` (`indexing_failed`) and handed to cleanup.
    Failed,
    /// The row was deleted, left `uploaded` or was handed to cleanup: no changes.
    Stopped,
    /// `ready` could not be written; the row stays `uploaded` for the reaper.
    SetReadyFailed,
    /// Gear stop.
    Cancelled,
}

impl IndexingOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::SetReadyFailed => "set_ready_failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Start the background indexing task of `job`.
#[must_use = "the handle reports the outcome; drop it to detach the task"]
pub fn spawn(deps: IndexingDeps, job: IndexingJob) -> JoinHandle<IndexingOutcome> {
    tokio::spawn(async move {
        let stop = deps.stop.clone();
        let outcome = tokio::select! {
            biased;
            () = stop.cancelled() => IndexingOutcome::Cancelled,
            o = run(&deps, &job) => o,
        };
        info!(
            attachment_id = %job.attachment_id,
            result = outcome.as_str(),
            "background indexing finished"
        );
        outcome
    })
}

async fn run(d: &IndexingDeps, job: &IndexingJob) -> IndexingOutcome {
    let t = d.timings;
    let limit = Instant::now() + t.background_limit;
    let mut poll = Poll {
        wait: t.poll_initial,
        last_transient: None,
    };
    loop {
        let now = Instant::now();
        if now >= limit {
            warn!(
                attachment_id = %job.attachment_id,
                last_transient_error = poll.last_transient.as_deref().unwrap_or("none"),
                "background indexing timed out"
            );
            return fail(d, job).await;
        }
        match heartbeat(d, job).await {
            Ok(true) => {}
            Ok(false) => return IndexingOutcome::Stopped,
            Err(e) => {
                warn!(attachment_id = %job.attachment_id, error = %e, "indexing heartbeat failed");
            }
        }
        if let Some(outcome) = round(d, job, &mut poll, (now + t.background_round).min(limit)).await
        {
            return outcome;
        }
    }
}

/// Backoff and the last transient read error, carried across rounds.
struct Poll {
    wait: std::time::Duration,
    last_transient: Option<String>,
}

/// Status reads until `round_end`; `Some` when indexing reached an outcome.
#[allow(clippy::cognitive_complexity)] // tracing macros inflate the score
async fn round(
    d: &IndexingDeps,
    job: &IndexingJob,
    poll: &mut Poll,
    round_end: Instant,
) -> Option<IndexingOutcome> {
    let t = d.timings;
    loop {
        let now = Instant::now();
        if now >= round_end {
            return None;
        }
        tokio::time::sleep(poll.wait.min(round_end - now)).await;
        poll.wait = (poll.wait * 2).min(t.background_poll_max);
        // Bounded by the round (itself bounded by the overall limit), so the
        // next heartbeat is never delayed past one round.
        let left = round_end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let read = tokio::time::timeout(
            left,
            d.rag
                .file_status(&job.provider, &job.vector_store_id, &job.provider_file_id),
        )
        .await;
        match read {
            Ok(Ok(VsFileStatus::Completed)) => return Some(set_ready(d, job).await),
            Ok(Ok(VsFileStatus::Failed)) => return Some(fail(d, job).await),
            Ok(Ok(VsFileStatus::InProgress)) => {}
            Ok(Err(e)) if e.is_transient() => {
                if poll.last_transient.is_none() {
                    warn!(attachment_id = %job.attachment_id, error = %e, "transient indexing status read error");
                }
                poll.last_transient = Some(e.to_string());
            }
            Ok(Err(e)) => {
                warn!(attachment_id = %job.attachment_id, error = %e, "indexing status read failed");
                return Some(fail(d, job).await);
            }
            Err(_elapsed) => poll.last_transient = Some("status read timed out".to_owned()),
        }
    }
}

async fn heartbeat(d: &IndexingDeps, job: &IndexingJob) -> Result<bool, DomainError> {
    AttachmentRepo::touch_uploaded(&d.db.conn()?, job.tenant_id, job.attachment_id, now_utc()).await
}

async fn set_ready(d: &IndexingDeps, job: &IndexingJob) -> IndexingOutcome {
    let mut wait = d.timings.set_ready_retry;
    for attempt in 1..=SET_READY_ATTEMPTS {
        let res = match d.db.conn() {
            Ok(conn) => {
                AttachmentRepo::mark_ready(&conn, job.tenant_id, job.attachment_id, None, now_utc())
                    .await
            }
            Err(e) => Err(e),
        };
        match res {
            Ok(true) => return IndexingOutcome::Ready,
            Ok(false) => return IndexingOutcome::Stopped,
            Err(e) => {
                warn!(attachment_id = %job.attachment_id, attempt, error = %e, "cannot mark the attachment ready");
                if attempt < SET_READY_ATTEMPTS {
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                }
            }
        }
    }
    error!(attachment_id = %job.attachment_id, "giving up marking the attachment ready");
    IndexingOutcome::SetReadyFailed
}

/// One transaction: `failed` + `cleanup_status = 'pending'` + cleanup message.
async fn fail(d: &IndexingDeps, job: &IndexingJob) -> IndexingOutcome {
    let outbox = Arc::clone(&d.outbox);
    let job2 = job.clone();
    let res = with_tx_retry(&d.db, "indexing failure", move |tx| {
        let (job2, outbox) = (job2.clone(), Arc::clone(&outbox));
        Box::pin(async move {
            let now = now_utc();
            if !AttachmentRepo::fail_indexing_for_cleanup(
                tx,
                job2.tenant_id,
                job2.attachment_id,
                now,
            )
            .await?
            {
                return Ok(None);
            }
            let payload = AttachmentCleanupPayload {
                event_type: AttachmentCleanupEvent::AttachmentIndexingFailed,
                tenant_id: job2.tenant_id,
                chat_id: job2.chat_id,
                attachment_id: job2.attachment_id,
                provider_file_id: Some(job2.provider_file_id),
                vector_store_id: None,
                storage_backend: job2.storage_backend,
                attachment_kind: job2.attachment_kind,
                deleted_at: now,
                secondary_ref: None,
            };
            outbox
                .enqueue_json(
                    tx,
                    QueueKind::AttachmentCleanup,
                    job2.tenant_id,
                    ATTACHMENT_CLEANUP_PAYLOAD_TYPE,
                    &payload,
                )
                .await
                .map(Some)
        })
    })
    .await;
    match res {
        Ok(Some(wake)) => {
            wake.fire();
            IndexingOutcome::Failed
        }
        Ok(None) => IndexingOutcome::Stopped,
        Err(e) => {
            error!(attachment_id = %job.attachment_id, error = %e, "cannot mark the attachment failed");
            IndexingOutcome::Stopped
        }
    }
}
