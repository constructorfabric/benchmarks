//! Document indexing: the chat's vector store (DESIGN "Creation protocol"), the wait for
//! indexing inside the upload request and the background task that keeps waiting after the
//! request deadline (DESIGN "File Upload", B.9.5).

use std::sync::Arc;
use std::time::Duration;

use opentelemetry::KeyValue;
use sea_orm::ActiveValue::Set;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::AccessScope;
use uuid::Uuid;

use super::AttachmentService;
use crate::config::MIN_UPLOAD_STALE_AFTER_SECS;
use crate::domain::error::DomainError;
use crate::infra::db::AttachmentKind;
use crate::infra::db::entity::chat_vector_stores;
use crate::infra::db::repo;
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::{write_tx, write_tx_with_wakes};
use crate::infra::llm::StorageTarget;
use crate::infra::outbox::payloads::AttachmentCleanupEvent;
use crate::infra::outbox::{OutboxEnqueuer, OutboxRecord};
use crate::infra::storage::{IndexStatus, StorageError, VectorStores};
use crate::metrics::Metrics;

/// Length of a background round in seconds (DESIGN: 20 s).
const BACKGROUND_ROUND_SECS: u64 = 20;
// The heartbeat must refresh `updated_at` well before the upload reaper may take the row.
const _: () = assert!(BACKGROUND_ROUND_SECS * 2 <= MIN_UPLOAD_STALE_AFTER_SECS);

/// A `NULL` placeholder older than this was left by a dead creator and is reclaimed.
const STALE_PLACEHOLDER: time::Duration = time::Duration::seconds(120);
/// Polls of a request that lost the placeholder insert before it answers 503.
const LOSER_POLLS: u32 = 5;
/// Rounds of the creation protocol (a reclaimed stale placeholder restarts it).
const PROTOCOL_ATTEMPTS: u32 = 3;
/// Waits between the attempts to set `ready` in the background (4 attempts in total).
const SET_READY_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
/// `error_code` of a failed vector store creation or lookup.
const ERROR_VECTOR_STORE_FAILED: &str = "vector_store_failed";
/// `error_code` of a failed or timed-out indexing.
const ERROR_INDEXING_FAILED: &str = "indexing_failed";
/// `event_type` of the cleanup of a document whose background indexing failed.
const EVENT_INDEXING_FAILED: &str = "attachment_indexing_failed";

/// Waits and deadlines of document indexing. [`Default`] gives the DESIGN values; tests use
/// smaller ones through `AttachmentService::with_timings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexingTimings {
    /// The upload waits for indexing until this long after the request started (25 s).
    pub request_deadline: Duration,
    /// First wait between status reads (250 ms), doubled after each read.
    pub request_poll_initial: Duration,
    /// Longest wait between status reads inside the request (2 s).
    pub request_poll_max: Duration,
    /// The background task gives up this long after it started (10 min).
    pub background_total: Duration,
    /// Length of one background round; each round first refreshes `updated_at` (20 s).
    pub background_round: Duration,
    /// Longest wait between status reads of the background task (5 s).
    pub background_poll_max: Duration,
}

impl Default for IndexingTimings {
    fn default() -> Self {
        Self {
            request_deadline: Duration::from_secs(25),
            request_poll_initial: Duration::from_millis(250),
            request_poll_max: Duration::from_secs(2),
            background_total: Duration::from_secs(600),
            background_round: Duration::from_secs(BACKGROUND_ROUND_SECS),
            background_poll_max: Duration::from_secs(5),
        }
    }
}

/// An uploaded document to index.
#[derive(Clone)]
pub(super) struct IndexJob {
    /// Tenant scope of the chat's child rows.
    pub scope: AccessScope,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub file_id: String,
    pub target: StorageTarget,
}

/// How a status wait ended.
enum Waited {
    Completed,
    Failed(String),
    /// Still in progress when the time ran out.
    Deadline,
}

impl AttachmentService {
    /// Adds the uploaded document to the chat's vector store and waits for indexing until
    /// `deadline`: `ready`, or `uploaded` with a background task when indexing is still running.
    /// A failure marks the row `failed` and deletes the provider file (best effort).
    ///
    /// # Errors
    /// `ProviderMismatch`, `StorageUnavailable`, database failures.
    pub(super) async fn index_document(
        &self,
        job: IndexJob,
        deadline: Instant,
    ) -> Result<(), DomainError> {
        let vs_id = match self.ensure_vector_store(&job).await {
            Ok(id) => id,
            Err(err) => {
                self.fail_and_delete_file(&job, ERROR_VECTOR_STORE_FAILED, &err.to_string())
                    .await;
                return Err(err);
            }
        };
        let added = self
            .deps
            .vector_stores
            .add_file(&job.target, &vs_id, &job.file_id, job.attachment_id)
            .await;
        let waited = match added {
            Ok(IndexStatus::Completed) => Waited::Completed,
            Ok(IndexStatus::Failed(reason)) => Waited::Failed(reason),
            Ok(IndexStatus::InProgress) => self.wait_in_request(&job, &vs_id, deadline).await,
            Err(err) => Waited::Failed(err.to_string()),
        };
        match waited {
            Waited::Completed => {
                self.set_ready(&job.scope, job.chat_id, job.attachment_id, None)
                    .await
            }
            Waited::Failed(reason) => {
                self.fail_and_delete_file(&job, ERROR_INDEXING_FAILED, &reason)
                    .await;
                Err(DomainError::StorageUnavailable(reason))
            }
            Waited::Deadline => {
                self.spawn_background(job, vs_id);
                Ok(())
            }
        }
    }

    /// Hands the wait to a [`BackgroundIndexing`] task (cancelled by [`Self::shutdown`]).
    fn spawn_background(&self, job: IndexJob, vs_id: String) {
        BackgroundIndexing {
            db: Arc::clone(&self.deps.db),
            vector_stores: Arc::clone(&self.deps.vector_stores),
            outbox: Arc::clone(&self.deps.outbox),
            metrics: Arc::clone(&self.deps.metrics),
            timings: self.timings,
            job,
            vs_id,
        }
        .spawn(self.cancel.child_token());
    }

    /// Polls the indexing status (250 ms doubling up to 2 s) until a final status or `deadline`;
    /// each read is bounded by the deadline. Transient read errors keep polling.
    async fn wait_in_request(&self, job: &IndexJob, vs_id: &str, deadline: Instant) -> Waited {
        let mut delay = self.timings.request_poll_initial;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Waited::Deadline;
            }
            tokio::time::sleep(delay.min(deadline - now)).await;
            let read = self
                .deps
                .vector_stores
                .file_status(&job.target, vs_id, &job.file_id);
            let Ok(status) =
                tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), read)
                    .await
            else {
                return Waited::Deadline;
            };
            match status {
                Ok(IndexStatus::Completed) => return Waited::Completed,
                Ok(IndexStatus::InProgress) => {}
                Ok(IndexStatus::Failed(reason)) => return Waited::Failed(reason),
                Err(StorageError::Transient(reason)) => {
                    tracing::debug!(attachment_id = %job.attachment_id, %reason, "transient indexing status error");
                }
                Err(err @ StorageError::Permanent { .. }) => {
                    return Waited::Failed(err.to_string());
                }
            }
            delay = (delay * 2).min(self.timings.request_poll_max);
        }
    }

    /// Marks the row `failed` and deletes its provider file in the background (best effort,
    /// not retried).
    async fn fail_and_delete_file(&self, job: &IndexJob, error_code: &'static str, reason: &str) {
        tracing::warn!(attachment_id = %job.attachment_id, error_code, %reason, "document indexing failed");
        self.mark_failed(&job.scope, job.attachment_id, error_code)
            .await;
        self.delete_file_best_effort(&job.target, &job.file_id, job.attachment_id);
    }

    /// The chat's provider vector store, created on first use (DESIGN "Creation protocol"):
    /// reuse an existing id; otherwise the request that inserts the `NULL` placeholder creates the
    /// store and sets its id (compare-and-set), the others poll for it; a stale placeholder is
    /// reclaimed.
    ///
    /// # Errors
    /// `ProviderMismatch` when the chat's store belongs to another backend, `StorageUnavailable`
    /// when the store cannot be created or does not appear in time, database failures.
    async fn ensure_vector_store(&self, job: &IndexJob) -> Result<String, DomainError> {
        for _ in 0..PROTOCOL_ATTEMPTS {
            let existing = {
                let conn = self.deps.db.conn()?;
                repo::vector_stores::find(&conn, &job.scope, job.chat_id).await?
            };
            let Some(row) = existing else {
                let row_id = Uuid::new_v4();
                return match self.insert_placeholder(job, row_id).await {
                    Ok(()) => self.create_vector_store(job, row_id).await,
                    Err(DomainError::Conflict { .. }) => self.await_vector_store(job).await,
                    Err(err) => Err(err),
                };
            };
            check_backend(&row, &job.target)?;
            if let Some(id) = row.vector_store_id {
                return Ok(id);
            }
            let cutoff = db_now() - STALE_PLACEHOLDER;
            if row.created_at >= cutoff {
                return self.await_vector_store(job).await;
            }
            tracing::warn!(chat_id = %job.chat_id, "reclaiming a stale vector store placeholder");
            self.delete_placeholder(job, row.id, cutoff).await?;
        }
        Err(DomainError::StorageUnavailable(
            "the chat vector store did not settle".to_owned(),
        ))
    }

    /// Winner path: creates the provider store and records its id on placeholder `row_id`.
    async fn create_vector_store(
        &self,
        job: &IndexJob,
        row_id: Uuid,
    ) -> Result<String, DomainError> {
        let name = format!("mini-chat-{}", job.chat_id);
        let vs_id = match self.deps.vector_stores.create(&job.target, &name).await {
            Ok(id) => id,
            Err(err) => {
                let far_future = db_now() + time::Duration::days(1);
                if let Err(e) = self.delete_placeholder(job, row_id, far_future).await {
                    tracing::warn!(chat_id = %job.chat_id, error = %e, "failed to remove the vector store placeholder");
                }
                return Err(DomainError::StorageUnavailable(err.to_string()));
            }
        };
        let (scope, chat_id, id) = (job.scope.clone(), job.chat_id, vs_id.clone());
        let set = write_tx(&self.deps.db, move |tx| {
            let (scope, id) = (scope.clone(), id.clone());
            Box::pin(async move {
                repo::vector_stores::set_vector_store_id(tx, &scope, chat_id, row_id, &id).await
            })
        })
        .await?;
        if set {
            return Ok(vs_id);
        }
        // The placeholder was reclaimed meanwhile: drop the new store, use the chat's one.
        let (stores, target) = (Arc::clone(&self.deps.vector_stores), job.target.clone());
        tokio::spawn(async move {
            if let Err(err) = stores.delete(&target, &vs_id).await {
                tracing::warn!(error = %err, "failed to delete a superseded vector store");
            }
        });
        self.await_vector_store(job).await
    }

    /// Loser path: polls the chat's row (backoff from the request poll interval) until the
    /// store id appears; 503 after [`LOSER_POLLS`] polls. Never creates a store.
    async fn await_vector_store(&self, job: &IndexJob) -> Result<String, DomainError> {
        let mut delay = self.timings.request_poll_initial;
        for _ in 0..LOSER_POLLS {
            tokio::time::sleep(delay).await;
            let row = {
                let conn = self.deps.db.conn()?;
                repo::vector_stores::find(&conn, &job.scope, job.chat_id).await?
            };
            if let Some(row) = row {
                check_backend(&row, &job.target)?;
                if let Some(id) = row.vector_store_id {
                    return Ok(id);
                }
            }
            delay *= 2;
        }
        Err(DomainError::StorageUnavailable(
            "the chat vector store is still being created".to_owned(),
        ))
    }

    /// Inserts the creation placeholder (its own transaction).
    async fn insert_placeholder(&self, job: &IndexJob, row_id: Uuid) -> Result<(), DomainError> {
        let row = chat_vector_stores::ActiveModel {
            id: Set(row_id),
            tenant_id: Set(job.tenant_id),
            chat_id: Set(job.chat_id),
            vector_store_id: Set(None),
            provider: Set(job.target.storage_backend.clone()),
            file_count: Set(0),
            created_at: Set(db_now()),
        };
        let scope = job.scope.clone();
        write_tx(&self.deps.db, move |tx| {
            let (scope, row) = (scope.clone(), row.clone());
            Box::pin(async move { repo::vector_stores::insert_placeholder(tx, &scope, row).await })
        })
        .await
    }

    async fn delete_placeholder(
        &self,
        job: &IndexJob,
        row_id: Uuid,
        created_before: time::OffsetDateTime,
    ) -> Result<(), DomainError> {
        let (scope, chat_id) = (job.scope.clone(), job.chat_id);
        write_tx(&self.deps.db, move |tx| {
            let scope = scope.clone();
            Box::pin(async move {
                repo::vector_stores::delete_placeholder(tx, &scope, chat_id, row_id, created_before)
                    .await
                    .map(drop)
            })
        })
        .await
    }
}

fn check_backend(
    row: &chat_vector_stores::Model,
    target: &StorageTarget,
) -> Result<(), DomainError> {
    if row.provider == target.storage_backend {
        Ok(())
    } else {
        Err(DomainError::ProviderMismatch)
    }
}

/// The wait for indexing after the upload answered `uploaded` (not persisted; cancelled on gear
/// stop).
struct BackgroundIndexing {
    db: Arc<DBProvider<DomainError>>,
    vector_stores: Arc<dyn VectorStores>,
    outbox: Arc<OutboxEnqueuer>,
    metrics: Arc<Metrics>,
    timings: IndexingTimings,
    job: IndexJob,
    vs_id: String,
}

/// Why the background task fails the row.
enum Failure {
    /// A failed / cancelled / unknown status or a non-transient read error.
    Failed(String),
    /// `background_total` elapsed; carries the last transient read error, if any.
    Timeout(Option<String>),
}

/// Poll state of the background task, kept across rounds.
struct Polling {
    delay: Duration,
    last_transient: Option<String>,
}

impl Polling {
    /// Remembers a transient read error (the first one is logged).
    fn transient(&mut self, attachment_id: Uuid, reason: String) {
        if self.last_transient.is_none() {
            tracing::warn!(%attachment_id, %reason, "transient indexing status error; still waiting");
        }
        self.last_transient = Some(reason);
    }
}

impl BackgroundIndexing {
    fn spawn(self, cancel: CancellationToken) {
        tokio::spawn(async move {
            let attachment_id = self.job.attachment_id;
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!(%attachment_id, "background indexing cancelled");
                }
                () = self.run() => {}
            }
        });
    }

    /// Rounds of `background_round`: refresh `updated_at` (stop when the row is no longer a live
    /// `uploaded` row), then poll the status (backoff up to `background_poll_max`) until the
    /// round ends; `background_total` after the start the row fails with a timeout.
    async fn run(self) {
        let t = self.timings;
        let end = Instant::now() + t.background_total;
        let mut poll = Polling {
            delay: t.request_poll_initial,
            last_transient: None,
        };
        loop {
            if Instant::now() >= end {
                self.fail(Failure::Timeout(poll.last_transient)).await;
                return;
            }
            if !self.heartbeat().await {
                return;
            }
            let round_end = (Instant::now() + t.background_round).min(end);
            match self.poll_round(round_end, end, &mut poll).await {
                Some(IndexStatus::Completed) => return self.set_ready().await,
                Some(IndexStatus::Failed(reason)) => {
                    return self.fail(Failure::Failed(reason)).await;
                }
                Some(IndexStatus::InProgress) | None => {}
            }
        }
    }

    /// Status reads until `round_end`; `Some` with a final status (a non-transient read error
    /// counts as `Failed`), `None` when the round (or the whole wait) ended first.
    async fn poll_round(
        &self,
        round_end: Instant,
        end: Instant,
        poll: &mut Polling,
    ) -> Option<IndexStatus> {
        while Instant::now() < round_end {
            tokio::time::sleep(
                poll.delay
                    .min(round_end.saturating_duration_since(Instant::now())),
            )
            .await;
            poll.delay = (poll.delay * 2).min(self.timings.background_poll_max);
            let read =
                self.vector_stores
                    .file_status(&self.job.target, &self.vs_id, &self.job.file_id);
            let status = tokio::time::timeout(end.saturating_duration_since(Instant::now()), read)
                .await
                .ok()?;
            match status {
                Ok(IndexStatus::InProgress) => {}
                Ok(done) => return Some(done),
                Err(StorageError::Transient(reason)) => {
                    poll.transient(self.job.attachment_id, reason);
                }
                Err(err @ StorageError::Permanent { .. }) => {
                    return Some(IndexStatus::Failed(err.to_string()));
                }
            }
        }
        None
    }

    /// Refreshes `updated_at`; `false` when the row is no longer a live `uploaded` row. A
    /// database failure is logged and the wait goes on.
    async fn heartbeat(&self) -> bool {
        let (scope, id) = (self.job.scope.clone(), self.job.attachment_id);
        let res = write_tx(&self.db, move |tx| {
            let scope = scope.clone();
            Box::pin(
                async move { repo::attachments::touch_uploaded(tx, &scope, id, db_now()).await },
            )
        })
        .await;
        match res {
            Ok(live) => {
                if !live {
                    tracing::debug!(attachment_id = %id, "row no longer indexable; background indexing stops");
                }
                live
            }
            Err(err) => {
                tracing::warn!(attachment_id = %id, error = %err, "background indexing heartbeat failed");
                true
            }
        }
    }

    /// Sets `ready` (4 attempts, 1 / 2 / 4 s apart). A row deleted or claimed meanwhile stays.
    async fn set_ready(&self) {
        let id = self.job.attachment_id;
        let mut delays = SET_READY_RETRY_DELAYS.iter();
        let result = loop {
            match self.try_set_ready().await {
                Ok(set) => break Some(set),
                Err(err) => {
                    tracing::warn!(attachment_id = %id, error = %err, "setting the indexed document ready failed");
                    match delays.next() {
                        Some(delay) => tokio::time::sleep(*delay).await,
                        None => break None,
                    }
                }
            }
        };
        match result {
            Some(true) => self.count("ready"),
            Some(false) => {}
            None => self.count("set_ready_failed"),
        }
    }

    async fn try_set_ready(&self) -> Result<bool, DomainError> {
        let (scope, id) = (self.job.scope.clone(), self.job.attachment_id);
        write_tx(&self.db, move |tx| {
            let scope = scope.clone();
            Box::pin(
                async move { repo::attachments::set_ready(tx, &scope, id, None, db_now()).await },
            )
        })
        .await
    }

    /// Fails the row for `failure` (see [`Self::fail_tx`]), then logs and counts the outcome.
    async fn fail(&self, failure: Failure) {
        let id = self.job.attachment_id;
        match self.fail_tx().await {
            Ok(false) => {}
            Ok(true) => {
                // A timeout names the last transient status read error, if any.
                let (result, reason) = match failure {
                    Failure::Failed(reason) => ("failed", Some(reason)),
                    Failure::Timeout(last_transient) => ("timeout", last_transient),
                };
                tracing::warn!(attachment_id = %id, result, ?reason, "background indexing failed");
                self.count(result);
            }
            Err(err) => {
                tracing::error!(attachment_id = %id, error = %err, "failed to record the background indexing failure");
            }
        }
    }

    /// One transaction: the row becomes `failed` / `indexing_failed` / cleanup `pending` and the
    /// attachment cleanup event is enqueued; `false` (nothing changed) when the row is no longer a
    /// live `uploaded` row.
    async fn fail_tx(&self) -> Result<bool, DomainError> {
        let job = self.job.clone();
        let outbox = Arc::clone(&self.outbox);
        write_tx_with_wakes(&self.db, move |tx, wakes| {
            let (job, outbox) = (job.clone(), Arc::clone(&outbox));
            Box::pin(async move {
                let now = db_now();
                let failed = repo::attachments::fail_for_cleanup(
                    tx,
                    &job.scope,
                    job.attachment_id,
                    ERROR_INDEXING_FAILED,
                    now,
                )
                .await?;
                if !failed {
                    return Ok(false);
                }
                let record = OutboxRecord::attachment_cleanup(&AttachmentCleanupEvent {
                    event_type: EVENT_INDEXING_FAILED.to_owned(),
                    tenant_id: job.tenant_id,
                    chat_id: job.chat_id,
                    attachment_id: job.attachment_id,
                    provider_file_id: Some(job.file_id.clone()),
                    vector_store_id: None,
                    storage_backend: job.target.storage_backend.clone(),
                    attachment_kind: AttachmentKind::Document.as_str().to_owned(),
                    deleted_at: now,
                    secondary_ref: None,
                })?;
                wakes.add(outbox.enqueue(tx, record).await?);
                Ok(true)
            })
        })
        .await
    }

    fn count(&self, result: &'static str) {
        self.metrics
            .attachment_background_indexing
            .add(1, &[KeyValue::new("result", result)]);
    }
}
