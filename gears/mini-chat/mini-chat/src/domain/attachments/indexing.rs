//! Document indexing wait: in-request polling until the 25 s deadline and the background task
//! that finishes indexing afterwards (DESIGN §3.6 "File Upload", B.9.5).

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::secure::SecureUpdateExt;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::attachment;
use crate::infra::llm::resolver::ResolvedProvider;
use crate::infra::llm::storage::{self, IndexStatus, StorageError};

use super::{
    CLEANUP_PENDING, ERR_INDEXING_FAILED, EVENT_INDEXING_FAILED, STATUS_FAILED, STATUS_UPLOADED, cleanup_payload,
    enqueue_cleanup, find_by_id, set_ready, system_ctx,
};

/// Heartbeat (round) interval of the background task, in seconds.
pub const BACKGROUND_ROUND_SECS: u64 = 20;
/// Minimum `upload_reaper.stale_after_secs`.
pub const MIN_STALE_AFTER_SECS: u64 = 60;
const _: () = assert!(BACKGROUND_ROUND_SECS * 2 <= MIN_STALE_AFTER_SECS, "heartbeat must be at most half the stale threshold");

/// Timings of the indexing wait (DESIGN values by default; shortened in tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexingTimings {
    /// Polling stops this long after the upload request started.
    pub request_deadline: Duration,
    /// First wait between status reads.
    pub poll_initial: Duration,
    /// Maximum wait between reads inside the request.
    pub request_poll_max: Duration,
    /// Background round length (each round refreshes `updated_at`).
    pub background_round: Duration,
    /// Maximum wait between reads in the background task.
    pub background_poll_max: Duration,
    /// Background task lifetime.
    pub background_total: Duration,
    /// First retry delay of the `ready` write (doubling, 4 attempts in total).
    pub ready_retry_base: Duration,
}

impl Default for IndexingTimings {
    fn default() -> Self {
        Self {
            request_deadline: Duration::from_secs(25),
            poll_initial: Duration::from_millis(250),
            request_poll_max: Duration::from_secs(2),
            background_round: Duration::from_secs(BACKGROUND_ROUND_SECS),
            background_poll_max: Duration::from_secs(5),
            background_total: Duration::from_secs(600),
            ready_retry_base: Duration::from_secs(1),
        }
    }
}

impl IndexingTimings {
    /// Short timings for tests.
    #[must_use]
    pub fn for_tests() -> Self {
        Self {
            request_deadline: Duration::from_millis(300),
            poll_initial: Duration::from_millis(20),
            request_poll_max: Duration::from_millis(50),
            background_round: Duration::from_millis(100),
            background_poll_max: Duration::from_millis(40),
            background_total: Duration::from_millis(1500),
            ready_retry_base: Duration::from_millis(10),
        }
    }
}

type Overrides = Mutex<Vec<(Weak<AppServices>, IndexingTimings)>>;
static OVERRIDES: Overrides = Mutex::new(Vec::new());

/// Timings for an application instance (defaults unless overridden).
#[must_use]
pub fn timings(app: &AppServices) -> IndexingTimings {
    let list = OVERRIDES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    list.iter()
        .find(|(w, _)| w.strong_count() > 0 && std::ptr::eq(w.as_ptr(), app))
        .map_or_else(IndexingTimings::default, |(_, t)| *t)
}

/// Overrides the timings of one application instance (tests).
pub fn set_timings(app: &Arc<AppServices>, t: IndexingTimings) {
    let mut list = OVERRIDES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    list.retain(|(w, _)| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), Arc::as_ptr(app)));
    list.push((Arc::downgrade(app), t));
}

/// Result of the in-request wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitOutcome {
    Completed,
    Failed(String),
    /// Still `in_progress` at the deadline.
    Deadline,
}

/// Polls the vector store file status (250 ms doubling to 2 s) until `deadline`. Each read is
/// bounded by the deadline; transient read errors keep polling, other errors fail.
#[allow(clippy::too_many_arguments)]
pub async fn wait_in_request(
    app: &AppServices,
    ctx: &SecurityContext,
    provider: &ResolvedProvider,
    vector_store_id: &str,
    file_id: &str,
    initial: IndexStatus,
    deadline: Instant,
    t: &IndexingTimings,
) -> WaitOutcome {
    let mut status = initial;
    let mut delay = t.poll_initial;
    loop {
        match status {
            IndexStatus::Completed => return WaitOutcome::Completed,
            IndexStatus::Failed(s) => return WaitOutcome::Failed(s),
            IndexStatus::InProgress => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return WaitOutcome::Deadline;
        }
        tokio::time::sleep(delay.min(remaining)).await;
        delay = (delay * 2).min(t.request_poll_max);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return WaitOutcome::Deadline;
        }
        let read = storage::get_vector_store_file_status(app.transport.as_ref(), ctx, provider, vector_store_id, file_id);
        status = match tokio::time::timeout(remaining, read).await {
            Err(_) => return WaitOutcome::Deadline,
            Ok(Ok(s)) => s,
            Ok(Err(StorageError::Transient(e))) => {
                tracing::debug!(error = %e, "transient indexing status read error");
                IndexStatus::InProgress
            }
            Ok(Err(e)) => return WaitOutcome::Failed(e.to_string()),
        };
    }
}

/// Work item of the background indexing task.
#[derive(Debug, Clone)]
pub struct BackgroundJob {
    pub tenant_id: Uuid,
    pub attachment_id: Uuid,
    pub vector_store_id: String,
    pub provider_file_id: String,
    pub provider: ResolvedProvider,
}

/// Outcome of the background task (for tests / logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundOutcome {
    Ready,
    Failed(String),
    /// Row deleted, no longer `uploaded`, claimed by cleanup, or `ready` write failed.
    Stopped,
}

/// Spawns the background indexing task; it stops on gear shutdown.
pub fn spawn_background(app: Arc<AppServices>, job: BackgroundJob) {
    tokio::spawn(async move {
        let shutdown = app.shutdown.clone();
        tokio::select! {
            () = shutdown.cancelled() => {
                tracing::info!(attachment_id = %job.attachment_id, "background indexing stopped by shutdown");
            }
            outcome = run_background(&app, &job) => {
                tracing::info!(attachment_id = %job.attachment_id, ?outcome, "background indexing finished");
            }
        }
    });
}

/// Polls in rounds (each refreshing `updated_at`) for up to `background_total`.
#[allow(clippy::cognitive_complexity, reason = "tracing macro expansion only")]
pub async fn run_background(app: &Arc<AppServices>, job: &BackgroundJob) -> BackgroundOutcome {
    let t = timings(app);
    let ctx = system_ctx(job.tenant_id);
    let start = Instant::now();
    let end = start + t.background_total;
    let mut delay = t.poll_initial;
    let mut last_transient: Option<String> = None;
    loop {
        if Instant::now() >= end {
            let reason = match &last_transient {
                Some(e) => format!("indexing timed out (last read error: {e})"),
                None => "indexing timed out".to_owned(),
            };
            return fail(app, job, reason).await;
        }
        match heartbeat(app, job).await {
            Ok(true) => {}
            Ok(false) => return BackgroundOutcome::Stopped,
            Err(e) => tracing::warn!(attachment_id = %job.attachment_id, error = %e, "indexing heartbeat failed"),
        }
        let round_end = (Instant::now() + t.background_round).min(end);
        while Instant::now() < round_end {
            tokio::time::sleep(delay.min(round_end.saturating_duration_since(Instant::now()))).await;
            delay = (delay * 2).min(t.background_poll_max);
            let read = storage::get_vector_store_file_status(
                app.transport.as_ref(),
                &ctx,
                &job.provider,
                &job.vector_store_id,
                &job.provider_file_id,
            );
            match tokio::time::timeout(t.background_round, read).await {
                Ok(Ok(IndexStatus::Completed)) => return finish_ready(app, job, &t).await,
                Ok(Ok(IndexStatus::Failed(s))) => return fail(app, job, format!("indexing status {s}")).await,
                Ok(Ok(IndexStatus::InProgress)) => {}
                Ok(Err(StorageError::Transient(e))) => {
                    if last_transient.is_none() {
                        tracing::warn!(attachment_id = %job.attachment_id, error = %e, "transient indexing status read error");
                    }
                    last_transient = Some(e);
                }
                Ok(Err(e)) => return fail(app, job, e.to_string()).await,
                Err(_) => last_transient = Some("status read timed out".to_owned()),
            }
        }
    }
}

fn live_uploaded(id: Uuid) -> Condition {
    Condition::all()
        .add(attachment::Column::Id.eq(id))
        .add(attachment::Column::Status.eq(STATUS_UPLOADED))
        .add(attachment::Column::CleanupStatus.is_null())
        .add(attachment::Column::DeletedAt.is_null())
}

/// Refreshes `updated_at` while the row is still a live `uploaded` row.
async fn heartbeat(app: &AppServices, job: &BackgroundJob) -> Result<bool, DomainError> {
    let conn = app.db.conn()?;
    let res = attachment::Entity::update_many()
        .col_expr(attachment::Column::UpdatedAt, Expr::value(clock::now()))
        .filter(live_uploaded(job.attachment_id))
        .secure()
        .scope_with(&AccessScope::for_tenant(job.tenant_id))
        .exec(&conn)
        .await?;
    Ok(res.rows_affected == 1)
}

async fn finish_ready(app: &AppServices, job: &BackgroundJob, t: &IndexingTimings) -> BackgroundOutcome {
    let mut delay = t.ready_retry_base;
    for attempt in 1..=4 {
        match set_ready(app, job.tenant_id, job.attachment_id, None).await {
            Ok(true) => return BackgroundOutcome::Ready,
            Ok(false) => return BackgroundOutcome::Stopped,
            Err(e) => {
                tracing::warn!(attachment_id = %job.attachment_id, attempt, error = %e, "setting the attachment ready failed");
                if attempt < 4 {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
    }
    tracing::error!(attachment_id = %job.attachment_id, "set_ready_failed: the upload reaper will finish the row");
    BackgroundOutcome::Stopped
}

/// One transaction: `failed` / `indexing_failed` / cleanup `pending` + cleanup message.
#[allow(clippy::cognitive_complexity, reason = "tracing macro expansion only")]
async fn fail(app: &Arc<AppServices>, job: &BackgroundJob, reason: String) -> BackgroundOutcome {
    tracing::warn!(attachment_id = %job.attachment_id, %reason, "background indexing failed");
    let row = match find_by_id(app, job.tenant_id, job.attachment_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return BackgroundOutcome::Stopped,
        Err(e) => {
            tracing::error!(attachment_id = %job.attachment_id, error = %e, "loading the attachment failed");
            return BackgroundOutcome::Stopped;
        }
    };
    let res = crate::domain::tx::retry_contention(|| {
        let app2 = Arc::clone(app);
        let row = row.clone();
        app.db.transaction(move |tx| {
            Box::pin(async move {
                let now = clock::now();
                let upd = attachment::Entity::update_many()
                    .col_expr(attachment::Column::Status, Expr::value(STATUS_FAILED))
                    .col_expr(attachment::Column::ErrorCode, Expr::value(Some(ERR_INDEXING_FAILED.to_owned())))
                    .col_expr(attachment::Column::CleanupStatus, Expr::value(Some(CLEANUP_PENDING.to_owned())))
                    .col_expr(attachment::Column::CleanupUpdatedAt, Expr::value(Some(now)))
                    .col_expr(attachment::Column::UpdatedAt, Expr::value(now))
                    .filter(live_uploaded(row.id))
                    .secure()
                    .scope_with(&AccessScope::for_tenant(row.tenant_id))
                    .exec(tx)
                    .await?;
                if upd.rows_affected == 0 {
                    return Ok(None);
                }
                let payload = cleanup_payload(EVENT_INDEXING_FAILED, &row, now);
                Ok(Some(enqueue_cleanup(&app2, tx, &payload).await?))
            })
        })
    })
    .await;
    match res {
        Ok(Some(w)) => {
            w.fire();
            BackgroundOutcome::Failed(reason)
        }
        Ok(None) => BackgroundOutcome::Stopped,
        Err(e) => {
            tracing::error!(attachment_id = %job.attachment_id, error = %e, "recording the indexing failure failed");
            BackgroundOutcome::Stopped
        }
    }
}
