//! Background indexing of a document whose vector-store indexing was still
//! running at the upload deadline (D "File Upload", B.9.5, ADR-0007).
//!
//! Rounds of `bg_round`: each refreshes the row's `updated_at` (the upload
//! reaper skips rows refreshed within `stale_after_secs`), then reads the
//! indexing status with doubling waits up to `bg_poll_max`. `completed` →
//! `ready` (4 attempts); `failed` / `cancelled`, a non-transient read error
//! or the `bg_total` limit → `failed` (`indexing_failed`) +
//! `cleanup_status = pending` + an `attachment_indexing_failed` cleanup
//! event in one transaction. A row that is deleted, no longer `uploaded` or
//! claimed by a cleanup (chat deleted), and gear shutdown, stop the task
//! without changes. The task is not persisted.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;
use tracing::{info, warn};
use uuid::Uuid;

use crate::domain::clock::Clock;
use crate::domain::error::DomainError;
use crate::domain::ports::{IndexStatus, OutboxPort, PendingWakes, StorageError, StoragePort};
use crate::domain::services::attachment_service::{BG_ROUND, IndexingTimings};
use crate::infra::db::repos::AttachmentRepo;
use crate::infra::db::tx::with_retry;
use crate::infra::llm::provider_resolver::StorageTarget;
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::outbox::payloads::{AttachmentCleanupEventType, AttachmentCleanupPayload};

/// Shortest `upload_reaper.stale_after_secs` (B.9.5).
const MIN_STALE_AFTER_SECS: u64 = 60;
// The production heartbeat must be at most half the shortest reaper cutoff.
const _: () = assert!(BG_ROUND.as_secs() * 2 <= MIN_STALE_AFTER_SECS);

/// Services the task uses.
#[derive(Clone)]
pub struct IndexingDeps {
    pub db: Arc<DBProvider<DomainError>>,
    pub clock: Arc<dyn Clock>,
    pub storage: Arc<dyn StoragePort>,
    pub outbox: Arc<dyn OutboxPort>,
    pub timings: IndexingTimings,
    pub metrics: Arc<MiniChatMetrics>,
}

/// The attachment being indexed.
#[derive(Debug, Clone)]
pub struct IndexingJob {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub attachment_kind: String,
    pub storage_backend: String,
    pub vector_store_id: String,
    pub provider_file_id: String,
    pub storage: StorageTarget,
}

/// How the wait ended.
#[derive(Debug)]
enum Outcome {
    Ready,
    Failed(String),
    /// The `bg_total` limit elapsed.
    TimedOut(String),
    Stopped(&'static str),
}

/// Start the task; it ends on its own or when `cancel` fires.
#[must_use = "the task is detached when the handle is dropped"]
pub fn spawn(deps: IndexingDeps, job: IndexingJob, cancel: CancellationToken) -> JoinHandle<()> {
    // `attachments_pending` while the task runs (taken before the request's
    // own guard is released).
    let pending = deps.metrics.attachment_pending();
    tokio::spawn(async move {
        let _pending = pending;
        let outcome = tokio::select! {
            () = cancel.cancelled() => Outcome::Stopped("gear stop"),
            o = wait(&deps, &job) => o,
        };
        finish(&deps, &job, outcome, &cancel).await;
    })
}

/// Poll until a terminal status, the time limit or a stop condition.
async fn wait(deps: &IndexingDeps, job: &IndexingJob) -> Outcome {
    let t = deps.timings;
    let scope = AccessScope::for_tenant(job.tenant_id);
    let total_end = Instant::now() + t.bg_total;
    let mut last_transient: Option<String> = None;
    while Instant::now() < total_end {
        match heartbeat(deps, job, &scope).await {
            Ok(false) => return Outcome::Stopped("row no longer uploaded"),
            Ok(true) => {}
            Err(e) => log(job, "indexing heartbeat failed", &e),
        }
        let round_end = (Instant::now() + t.bg_round).min(total_end);
        let mut pause = t.poll_initial;
        while let Some(left) = round_end.checked_duration_since(Instant::now()) {
            tokio::time::sleep(pause.min(left)).await;
            pause = (pause * 2).min(t.bg_poll_max);
            let read = deps
                .storage
                .vector_store_file_status(&job.storage, &job.vector_store_id, &job.provider_file_id)
                .await;
            if let Some(outcome) = classify(job, read, &mut last_transient) {
                return outcome;
            }
        }
    }
    Outcome::TimedOut(format!(
        "indexing timed out; last transient error: {}",
        last_transient.as_deref().unwrap_or("none")
    ))
}

/// Terminal outcome of one status read (`None`: keep polling).
fn classify(
    job: &IndexingJob,
    read: Result<IndexStatus, StorageError>,
    last_transient: &mut Option<String>,
) -> Option<Outcome> {
    match read {
        Ok(IndexStatus::Completed) => Some(Outcome::Ready),
        Ok(IndexStatus::Failed) => Some(Outcome::Failed("indexing failed".to_owned())),
        Ok(IndexStatus::InProgress) => None,
        Err(StorageError::Transient(m)) => {
            // The first transient error is logged; a timeout names the last one.
            if last_transient.is_none() {
                log(job, "transient indexing status error", &m);
            }
            *last_transient = Some(m);
            None
        }
        Err(e @ StorageError::Permanent(_)) => Some(Outcome::Failed(e.to_string())),
    }
}

fn log(job: &IndexingJob, what: &str, e: &dyn std::fmt::Display) {
    warn!(error = %e, attachment_id = %job.attachment_id, "{what}");
}

/// Refresh `updated_at`; `false` when the row is no longer a live `uploaded` row.
async fn heartbeat(
    deps: &IndexingDeps,
    job: &IndexingJob,
    scope: &AccessScope,
) -> Result<bool, DomainError> {
    let conn = deps.db.conn()?;
    let n = AttachmentRepo
        .touch_uploaded(
            &conn,
            scope,
            job.chat_id,
            job.attachment_id,
            deps.clock.now(),
        )
        .await?;
    Ok(n > 0)
}

async fn finish(
    deps: &IndexingDeps,
    job: &IndexingJob,
    outcome: Outcome,
    cancel: &CancellationToken,
) {
    match outcome {
        Outcome::Ready => set_ready(deps, job, cancel).await,
        Outcome::Failed(reason) => fail(deps, job, "failed", &reason).await,
        Outcome::TimedOut(reason) => fail(deps, job, "timeout", &reason).await,
        Outcome::Stopped(why) => {
            info!(attachment_id = %job.attachment_id, reason = why, "background indexing stopped");
        }
    }
}

/// Record a failed / timed-out indexing (`result` = metric label).
async fn fail(deps: &IndexingDeps, job: &IndexingJob, result: &'static str, reason: &str) {
    deps.metrics.background_indexing(result);
    log(job, "background indexing failed", &reason);
    if let Err(e) = fail_with_cleanup(deps, job).await {
        log(job, "could not record the indexing failure", &e);
    }
}

/// `ready`, in up to 4 attempts; a row left `uploaded` is later reaped
/// (`upload_abandoned`).
async fn set_ready(deps: &IndexingDeps, job: &IndexingJob, cancel: &CancellationToken) {
    let mut delays = deps.timings.ready_retry_delays.into_iter();
    loop {
        match try_set_ready(deps, job).await {
            Ok(n) => {
                if n > 0 {
                    deps.metrics.background_indexing("ready");
                }
                return;
            }
            Err(e) => log(job, "setting ready failed", &e),
        }
        let Some(delay) = delays.next() else { break };
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
    deps.metrics.background_indexing("set_ready_failed");
    warn!(attachment_id = %job.attachment_id, result = "set_ready_failed", "attachment left uploaded");
}

/// One `ready` attempt (0 rows: the row is no longer a live `uploaded` row).
async fn try_set_ready(deps: &IndexingDeps, job: &IndexingJob) -> Result<u64, DomainError> {
    let conn = deps.db.conn()?;
    let scope = AccessScope::for_tenant(job.tenant_id);
    Ok(AttachmentRepo
        .mark_ready(
            &conn,
            &scope,
            job.chat_id,
            job.attachment_id,
            None,
            deps.clock.now(),
        )
        .await?)
}

/// `failed` + `cleanup_status = pending` + cleanup event, one transaction.
async fn fail_with_cleanup(deps: &IndexingDeps, job: &IndexingJob) -> Result<(), DomainError> {
    let scope = AccessScope::for_tenant(job.tenant_id);
    let now = deps.clock.now();
    let outbox = Arc::clone(&deps.outbox);
    let payload = AttachmentCleanupPayload {
        event_type: AttachmentCleanupEventType::AttachmentIndexingFailed,
        tenant_id: job.tenant_id,
        chat_id: job.chat_id,
        attachment_id: job.attachment_id,
        provider_file_id: Some(job.provider_file_id.clone()),
        vector_store_id: None,
        storage_backend: job.storage_backend.clone(),
        attachment_kind: job.attachment_kind.clone(),
        deleted_at: now,
        secondary_ref: None,
    };
    let (chat_id, attachment_id) = (job.chat_id, job.attachment_id);
    let wakes = with_retry(&deps.db, move |tx| {
        let (scope, outbox, payload) = (scope.clone(), Arc::clone(&outbox), payload.clone());
        Box::pin(async move {
            let mut wakes = PendingWakes::new();
            if AttachmentRepo
                .fail_indexing_for_cleanup(tx, &scope, chat_id, attachment_id, now)
                .await?
                == 1
            {
                outbox
                    .enqueue_attachment_cleanup(tx, &payload, &mut wakes)
                    .await?;
            }
            Ok(wakes)
        })
    })
    .await?;
    wakes.fire_all();
    Ok(())
}
