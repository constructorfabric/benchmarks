//! Document indexing (DESIGN 3.6 "File Upload"): the add to the chat vector
//! store, the request-phase wait for `completed`, and the background task that
//! keeps waiting when the request deadline passes.
//!
//! Request phase: the add answer is the first status; while it is
//! `in_progress` (or has no status) the status is read after waits that start
//! at `first_poll_delay` and double up to `max_poll_delay`, until
//! `request_deadline` after the request started. Each read is bounded by the
//! same deadline. `failed`, `cancelled`, any other status and a non-transient
//! read error fail the upload (503, the provider file is deleted inline once);
//! a transient read error keeps waiting.
//!
//! Background task: rounds of `background_round`, each starting with a refresh
//! of `updated_at` (so the upload reaper leaves the row alone); reads wait
//! `first_poll_delay` doubling up to `background_max_poll_delay`. The task
//! ends when the row is deleted, no longer `uploaded` or marked for cleanup
//! (no change), on gear stop (the row stays `uploaded`), on `completed`
//! (`ready`), and on failure or after `background_max` (one transaction: `failed`
//! + `cleanup_status = 'pending'` + the cleanup message).

use std::time::Instant;

use tokio_util::sync::CancellationToken;
use toolkit_db::secure::TxConfig;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::upload::{Job, unavailable};
use super::{
    AttachmentCleanupEvent, AttachmentCleanupKind, AttachmentService, IndexingTimings, Shared,
    error_code,
};
use crate::domain::clock::now_utc;
use crate::domain::error::DomainError;
use crate::domain::ports::{AttachmentLifecycleRepository, IndexStatus, StorageTarget};

/// The vector store file being indexed.
struct IndexFile {
    store: String,
    provider_file: String,
}

/// One status read, classified.
enum Read {
    Completed,
    Failed,
    /// `in_progress`, no status, or a transient error: keep waiting.
    Pending,
}

/// How the request-phase wait ended.
enum Wait {
    Completed,
    Failed,
    Deadline,
}

/// Reads the indexing status once and classifies the answer.
async fn read_status(
    shared: &Shared,
    ctx: &SecurityContext,
    target: &StorageTarget,
    file: &IndexFile,
) -> Read {
    match shared
        .storage
        .index_status(ctx, target, &file.store, &file.provider_file)
        .await
    {
        Ok(IndexStatus::Completed) => Read::Completed,
        Ok(IndexStatus::InProgress) => Read::Pending,
        Ok(IndexStatus::Failed) => Read::Failed,
        Err(error) if error.transient => {
            tracing::debug!(%error, "transient indexing status read error");
            Read::Pending
        }
        Err(error) => {
            tracing::warn!(%error, "indexing status read failed");
            Read::Failed
        }
    }
}

fn next_delay(delay: std::time::Duration, max: std::time::Duration) -> std::time::Duration {
    delay.saturating_mul(2).min(max)
}

impl AttachmentService {
    /// Adds the uploaded document to the chat vector store and waits for the
    /// indexing as far as the request deadline allows.
    pub(super) async fn index_document(
        &self,
        job: &Job<'_>,
        provider_file: &str,
    ) -> Result<(), DomainError> {
        let store = match self.ensure_vector_store(job).await {
            Ok(store) => store,
            Err(error) => {
                return Err(self
                    .fail_with(
                        job,
                        error_code::VECTOR_STORE_FAILED,
                        Some(provider_file),
                        error,
                    )
                    .await);
            }
        };
        let first = match self
            .shared
            .storage
            .add_file_to_store(
                job.ctx,
                &job.target,
                &store,
                provider_file,
                job.attachment_id,
            )
            .await
        {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(%error, attachment_id = %job.attachment_id,
                    "adding the file to the vector store failed");
                return Err(self
                    .fail_with(
                        job,
                        error_code::INDEXING_FAILED,
                        Some(provider_file),
                        unavailable(),
                    )
                    .await);
            }
        };
        let file = IndexFile {
            store,
            provider_file: provider_file.to_owned(),
        };
        let wait = match first {
            IndexStatus::Completed => Wait::Completed,
            IndexStatus::Failed => Wait::Failed,
            IndexStatus::InProgress => self.wait_in_request(job, &file).await,
        };
        match wait {
            Wait::Completed => self.make_ready(job, None).await,
            Wait::Failed => Err(self
                .fail_with(
                    job,
                    error_code::INDEXING_FAILED,
                    Some(provider_file),
                    unavailable(),
                )
                .await),
            Wait::Deadline => {
                self.spawn_background(job, file);
                Ok(())
            }
        }
    }

    /// The request-phase polling loop.
    async fn wait_in_request(&self, job: &Job<'_>, file: &IndexFile) -> Wait {
        let timings = self.shared.timings;
        let deadline = job.started + timings.request_deadline;
        let mut delay = timings.first_poll_delay;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Wait::Deadline;
            }
            tokio::time::sleep(delay.min(deadline - now)).await;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Wait::Deadline;
            }
            let read = tokio::time::timeout(
                remaining,
                read_status(&self.shared, job.ctx, &job.target, file),
            )
            .await;
            match read {
                Err(_elapsed) => return Wait::Deadline,
                Ok(Read::Completed) => return Wait::Completed,
                Ok(Read::Failed) => return Wait::Failed,
                Ok(Read::Pending) => {}
            }
            delay = next_delay(delay, timings.max_poll_delay);
        }
    }

    /// Hands the row to the background task.
    fn spawn_background(&self, job: &Job<'_>, file: IndexFile) {
        let task = Background {
            shared: self.shared.clone(),
            stop: self.stop.clone(),
            ctx: job.ctx.clone(),
            scope: job.scope.clone(),
            target: job.target.clone(),
            tenant_id: job.chat.tenant_id,
            chat_id: job.chat.id,
            attachment_id: job.attachment_id,
            file,
        };
        self.tasks.spawn(task.run());
    }
}

/// How one round of the background task ended.
enum Verdict {
    Completed,
    Failed,
    TimedOut,
    NextRound,
    Stopped,
}

/// The background indexing task of one attachment.
struct Background {
    shared: Shared,
    stop: CancellationToken,
    ctx: SecurityContext,
    scope: AccessScope,
    target: StorageTarget,
    tenant_id: Uuid,
    chat_id: Uuid,
    attachment_id: Uuid,
    file: IndexFile,
}

impl Background {
    async fn run(self) {
        let started = Instant::now();
        let timings = self.shared.timings;
        loop {
            if !self.refresh().await {
                return;
            }
            match self.round(started, timings).await {
                Verdict::Completed => return self.finish_ready().await,
                Verdict::Failed | Verdict::TimedOut => return self.finish_failed().await,
                Verdict::Stopped => return,
                Verdict::NextRound => {}
            }
        }
    }

    /// Refreshes `updated_at`; `false` when the task must stop without changes
    /// (the row was deleted, finished or taken by a cleanup).
    async fn refresh(&self) -> bool {
        let refreshed = match self.shared.db.conn() {
            Ok(conn) => {
                conn.refresh_uploaded_attachment(&self.scope, self.attachment_id, now_utc())
                    .await
            }
            Err(error) => Err(error),
        };
        match refreshed {
            Ok(still_ours) => still_ours,
            Err(error) => {
                tracing::warn!(attachment_id = %self.attachment_id, %error,
                    "background indexing could not refresh the row; going on");
                true
            }
        }
    }

    /// One round of reads.
    async fn round(&self, started: Instant, timings: IndexingTimings) -> Verdict {
        let round_end = Instant::now() + timings.background_round;
        let mut delay = timings.first_poll_delay;
        loop {
            if started.elapsed() >= timings.background_max {
                return Verdict::TimedOut;
            }
            let now = Instant::now();
            if now >= round_end {
                return Verdict::NextRound;
            }
            tokio::select! {
                () = self.stop.cancelled() => return Verdict::Stopped,
                () = tokio::time::sleep(delay.min(round_end - now)) => {}
            }
            let remaining = round_end.saturating_duration_since(Instant::now());
            let read = tokio::select! {
                () = self.stop.cancelled() => return Verdict::Stopped,
                read = tokio::time::timeout(
                    remaining,
                    read_status(&self.shared, &self.ctx, &self.target, &self.file),
                ) => read,
            };
            match read {
                Ok(Read::Completed) => return Verdict::Completed,
                Ok(Read::Failed) => return Verdict::Failed,
                Ok(Read::Pending) | Err(_) => {}
            }
            delay = next_delay(delay, timings.background_max_poll_delay);
        }
    }

    /// `uploaded` -> `ready`; a row that changed meanwhile is left alone.
    async fn finish_ready(&self) {
        let ready = match self.shared.db.conn() {
            Ok(conn) => {
                conn.mark_attachment_ready(&self.scope, self.attachment_id, None, now_utc())
                    .await
            }
            Err(error) => Err(error),
        };
        match ready {
            Ok(true) => {}
            Ok(false) => tracing::debug!(attachment_id = %self.attachment_id,
                "the indexed attachment changed meanwhile; left alone"),
            Err(error) => tracing::warn!(attachment_id = %self.attachment_id, %error,
                "the indexed attachment could not be made ready; the upload reaper will take it"),
        }
    }

    /// One transaction: `failed`, `cleanup_status = 'pending'` and the cleanup
    /// message. A failing enqueue rolls the whole change back (the row stays
    /// `uploaded` for the upload reaper).
    async fn finish_failed(&self) {
        let now = now_utc();
        let event = AttachmentCleanupEvent {
            event_type: AttachmentCleanupKind::IndexingFailed,
            tenant_id: self.tenant_id,
            chat_id: self.chat_id,
            attachment_id: self.attachment_id,
            provider_file_id: Some(self.file.provider_file.clone()),
            vector_store_id: None,
            storage_backend: self.target.backend.clone(),
            attachment_kind: "document".to_owned(),
            deleted_at: now,
            secondary_ref: None,
        };
        let result = self
            .shared
            .db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let (scope, event) = (self.scope.clone(), event.clone());
                let (id, cleanup) = (self.attachment_id, self.shared.cleanup.clone());
                Box::pin(async move {
                    if !tx.fail_indexing_with_cleanup(&scope, id, now).await? {
                        return Ok(None);
                    }
                    cleanup
                        .enqueue_attachment_cleanup(tx, &event)
                        .await
                        .map(Some)
                })
            })
            .await;
        match result {
            Ok(Some(wake)) => wake.fire(),
            Ok(None) => tracing::debug!(attachment_id = %self.attachment_id,
                "the attachment was decided by someone else; indexing failure not recorded"),
            Err(error) => tracing::warn!(attachment_id = %self.attachment_id, %error,
                "the indexing failure could not be recorded; the upload reaper will take the row"),
        }
    }
}
