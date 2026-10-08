//! Vector store indexing wait: in-request polling and the background task.

use std::sync::Arc;
use std::time::Instant;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::SecureUpdateExt;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{UploadTimings, cleanup_status, error_codes, status, status_reason};
use crate::domain::error::DomainError;
use crate::domain::outbox_payloads::{AttachmentCleanupEvent, attachment_event_types};
use crate::domain::service::Deps;
use crate::infra::db::entity::attachment;
use crate::infra::llm::VectorFileStatus;

/// Outcome of the in-request indexing wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SyncOutcome {
    Ready,
    /// Failed / cancelled / unknown status, or a non-transient read error.
    Failed(String),
    /// Still `in_progress` at the request deadline.
    StillIndexing,
}

/// Polls the vector store file status until `deadline` (250 ms doubling to 2 s by default).
/// Each status read is bounded by the same deadline; transient read errors keep polling.
#[allow(clippy::too_many_arguments)]
pub(super) async fn wait_in_request(
    deps: &Deps,
    timings: &UploadTimings,
    provider_id: &str,
    tenant_id: Uuid,
    vs_id: &str,
    file_id: &str,
    initial: VectorFileStatus,
    deadline: Instant,
) -> SyncOutcome {
    match initial {
        VectorFileStatus::Completed => return SyncOutcome::Ready,
        VectorFileStatus::Failed(r) => return SyncOutcome::Failed(r),
        VectorFileStatus::InProgress => {}
    }
    let mut delay = timings.sync_poll_initial;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return SyncOutcome::StillIndexing;
        }
        tokio::time::sleep(delay.min(deadline - now)).await;
        let now = Instant::now();
        if now >= deadline {
            return SyncOutcome::StillIndexing;
        }
        let read = tokio::time::timeout(
            deadline - now,
            deps.storage
                .get_vector_store_file_status(provider_id, tenant_id, vs_id, file_id),
        )
        .await;
        match read {
            Err(_) => return SyncOutcome::StillIndexing,
            Ok(Ok(VectorFileStatus::Completed)) => return SyncOutcome::Ready,
            Ok(Ok(VectorFileStatus::InProgress)) => {}
            Ok(Ok(s @ VectorFileStatus::Failed(_))) => return SyncOutcome::Failed(status_reason(&s)),
            Ok(Err(e)) if e.is_transient() => {
                tracing::debug!(error = %e, "transient vector store file status read error");
            }
            Ok(Err(e)) => return SyncOutcome::Failed(format!("status read error: {e}")),
        }
        delay = delay.saturating_mul(2).min(timings.sync_poll_max);
    }
}

/// Identity of a background indexing wait.
#[derive(Debug, Clone)]
pub(super) struct BackgroundJob {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub storage_provider_id: String,
    pub storage_backend: String,
    pub vector_store_id: String,
    pub provider_file_id: String,
}

impl BackgroundJob {
    fn scope(&self) -> AccessScope {
        AccessScope::for_tenant(self.tenant_id)
    }

    /// Row predicate: still `uploaded`, not marked for cleanup, not deleted.
    fn guard(&self) -> Condition {
        Condition::all()
            .add(attachment::Column::Id.eq(self.attachment_id))
            .add(attachment::Column::ChatId.eq(self.chat_id))
            .add(attachment::Column::Status.eq(status::UPLOADED))
            .add(attachment::Column::CleanupStatus.is_null())
            .add(attachment::Column::DeletedAt.is_null())
    }
}

/// Spawns the background wait on the gear task tracker (cancelled on gear stop).
pub(super) fn spawn_background(deps: Arc<Deps>, timings: UploadTimings, job: BackgroundJob) {
    let shutdown = deps.shutdown.clone();
    let tasks = deps.tasks.clone();
    tasks.spawn(async move {
        tokio::select! {
            () = shutdown.cancelled() => {
                tracing::info!(attachment_id = %job.attachment_id, "background indexing wait cancelled by shutdown");
            }
            () = run_background(&deps, &timings, &job) => {}
        }
    });
}

enum BgEnd {
    Completed,
    Failed(String),
    Stopped,
}

async fn run_background(deps: &Deps, timings: &UploadTimings, job: &BackgroundJob) {
    match poll_background(deps, timings, job).await {
        BgEnd::Completed => set_ready(deps, timings, job).await,
        BgEnd::Failed(reason) => {
            tracing::warn!(attachment_id = %job.attachment_id, reason = %reason, "background indexing failed");
            if let Err(e) = fail_and_enqueue(deps, job).await {
                tracing::error!(attachment_id = %job.attachment_id, error = %e, "failed to record background indexing failure");
            }
        }
        BgEnd::Stopped => {
            tracing::debug!(attachment_id = %job.attachment_id, "background indexing stopped: row no longer uploaded");
        }
    }
}

/// Refreshes `updated_at` (heartbeat for the upload reaper); 0 rows means "stop".
async fn heartbeat(deps: &Deps, job: &BackgroundJob) -> Result<u64, DomainError> {
    let conn = deps.db.conn()?;
    Ok(attachment::Entity::update_many()
        .col_expr(attachment::Column::UpdatedAt, Expr::value(OffsetDateTime::now_utc()))
        .filter(job.guard())
        .secure()
        .scope_with(&job.scope())
        .exec(&conn)
        .await?
        .rows_affected)
}

async fn poll_background(deps: &Deps, timings: &UploadTimings, job: &BackgroundJob) -> BgEnd {
    let started = Instant::now();
    let total_deadline = started + timings.background_total;
    let mut last_transient: Option<String> = None;
    loop {
        let now = Instant::now();
        if now >= total_deadline {
            return BgEnd::Failed(format!(
                "indexing timeout; last transient error: {}",
                last_transient.as_deref().unwrap_or("none")
            ));
        }
        match heartbeat(deps, job).await {
            Ok(0) => return BgEnd::Stopped,
            Ok(_) => {}
            Err(e) => tracing::warn!(attachment_id = %job.attachment_id, error = %e, "indexing heartbeat failed"),
        }
        let round_end = (now + timings.background_round).min(total_deadline);
        let mut delay = timings.background_poll_initial;
        loop {
            let now = Instant::now();
            if now >= round_end {
                break;
            }
            tokio::time::sleep(delay.min(round_end - now)).await;
            delay = delay.saturating_mul(2).min(timings.background_poll_max);
            let read = deps
                .storage
                .get_vector_store_file_status(
                    &job.storage_provider_id,
                    job.tenant_id,
                    &job.vector_store_id,
                    &job.provider_file_id,
                )
                .await;
            match read {
                Ok(VectorFileStatus::Completed) => return BgEnd::Completed,
                Ok(VectorFileStatus::InProgress) => {}
                Ok(s @ VectorFileStatus::Failed(_)) => return BgEnd::Failed(status_reason(&s)),
                Err(e) if e.is_transient() => {
                    if last_transient.is_none() {
                        tracing::warn!(attachment_id = %job.attachment_id, error = %e, "transient vector store status read error");
                    }
                    last_transient = Some(e.to_string());
                }
                Err(e) => return BgEnd::Failed(format!("status read error: {e}")),
            }
        }
    }
}

async fn try_set_ready(deps: &Deps, job: &BackgroundJob) -> Result<u64, DomainError> {
    let conn = deps.db.conn()?;
    Ok(attachment::Entity::update_many()
        .col_expr(attachment::Column::Status, Expr::value(status::READY))
        .col_expr(attachment::Column::UpdatedAt, Expr::value(OffsetDateTime::now_utc()))
        .filter(job.guard())
        .secure()
        .scope_with(&job.scope())
        .exec(&conn)
        .await?
        .rows_affected)
}

/// Sets `ready`; a failed write is retried after each `set_ready_retry` wait.
async fn set_ready(deps: &Deps, timings: &UploadTimings, job: &BackgroundJob) {
    let mut waits = timings.set_ready_retry.iter();
    loop {
        match try_set_ready(deps, job).await {
            Ok(n) => {
                if n == 0 {
                    tracing::debug!(attachment_id = %job.attachment_id, "indexing completed but row is no longer uploaded");
                }
                return;
            }
            Err(e) => {
                if let Some(w) = waits.next() {
                    tracing::warn!(attachment_id = %job.attachment_id, error = %e, "set ready failed; retrying");
                    tokio::time::sleep(*w).await;
                } else {
                    tracing::error!(attachment_id = %job.attachment_id, error = %e, result = "set_ready_failed", "set ready failed; the upload reaper will fail the row");
                    return;
                }
            }
        }
    }
}

/// One transaction: `failed` / `indexing_failed` / cleanup `pending` + cleanup outbox event.
async fn fail_and_enqueue(deps: &Deps, job: &BackgroundJob) -> Result<(), DomainError> {
    let outbox = Arc::clone(&deps.outbox);
    let job = job.clone();
    let wake = deps
        .db
        .transaction(move |tx| {
            let outbox = Arc::clone(&outbox);
            let job = job.clone();
            Box::pin(async move {
                let now = OffsetDateTime::now_utc();
                let affected = attachment::Entity::update_many()
                    .col_expr(attachment::Column::Status, Expr::value(status::FAILED))
                    .col_expr(
                        attachment::Column::ErrorCode,
                        Expr::value(error_codes::INDEXING_FAILED),
                    )
                    .col_expr(
                        attachment::Column::CleanupStatus,
                        Expr::value(cleanup_status::PENDING),
                    )
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                    .filter(job.guard())
                    .secure()
                    .scope_with(&job.scope())
                    .exec(tx)
                    .await?
                    .rows_affected;
                if affected == 0 {
                    return Ok(None);
                }
                let ev = AttachmentCleanupEvent {
                    event_type: attachment_event_types::INDEXING_FAILED.to_owned(),
                    tenant_id: job.tenant_id,
                    chat_id: job.chat_id,
                    attachment_id: job.attachment_id,
                    provider_file_id: Some(job.provider_file_id.clone()),
                    vector_store_id: None,
                    storage_backend: job.storage_backend.clone(),
                    attachment_kind: "document".to_owned(),
                    deleted_at: now,
                    secondary_ref: None,
                };
                let wake = outbox.enqueue_attachment_cleanup(tx, &ev).await?;
                Ok::<_, DomainError>(Some(wake))
            })
        })
        .await?;
    if let Some(w) = wake {
        w.fire();
    }
    Ok(())
}
