//! The indexing wait of a document upload inside the request (DESIGN "File Upload": poll the
//! vector store file until `completed`, a failure or the request deadline).

use std::sync::Arc;

use tokio::time::Instant;
use uuid::Uuid;

use super::background::BackgroundIndexing;
use super::model::AttachmentKind;
use super::ports::{IndexingStatus, StorageError};
use super::service::{AttachmentService, UploadSession};
use crate::domain::cleanup::{AttachmentCleanupEvent, AttachmentCleanupPayload};
use crate::domain::clock;

/// How the indexing wait of a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Indexing {
    /// The document is searchable.
    Completed,
    /// A failed, cancelled or unknown status, a non-transient read error or a failed add.
    Failed,
    /// Still `in_progress` at the request deadline.
    Running,
}

/// What one status read means for the wait.
enum Step {
    Done(Indexing),
    Continue,
}

/// Maps a status read to the next step of the wait: a transient read error keeps polling, a
/// failed status or a read error that retrying cannot fix ends it.
fn next_step(read: Result<IndexingStatus, StorageError>) -> Step {
    match read {
        Ok(IndexingStatus::Completed) => Step::Done(Indexing::Completed),
        Ok(IndexingStatus::InProgress) => Step::Continue,
        Err(err) if err.transient => Step::Continue,
        Ok(IndexingStatus::Failed(_)) | Err(_) => Step::Done(Indexing::Failed),
    }
}

impl AttachmentService {
    /// Waits for the vector store file until the request deadline: the first wait is
    /// `poll_initial`, every next one doubles up to `poll_max`. A response without a status counts
    /// as `in_progress`; a transient read error keeps polling.
    pub(super) async fn await_indexing(
        &self,
        session: &UploadSession,
        vector_store: &str,
        provider_file: &str,
        added: IndexingStatus,
    ) -> Indexing {
        match added {
            IndexingStatus::Completed => return Indexing::Completed,
            IndexingStatus::Failed(_) => return Indexing::Failed,
            IndexingStatus::InProgress => {}
        }
        let deadline = session.started + self.timings.request_deadline;
        let mut wait = self.timings.poll_initial;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Indexing::Running;
            }
            tokio::time::sleep(wait.min(left)).await;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Indexing::Running;
            }
            let read = tokio::time::timeout(
                left,
                self.storage.file_status(
                    &session.ctx,
                    &session.storage.primary,
                    vector_store,
                    provider_file,
                ),
            )
            .await;
            let Ok(read) = read else {
                return Indexing::Running;
            };
            if let Step::Done(done) = next_step(read) {
                return done;
            }
            wait = wait.saturating_mul(2).min(self.timings.poll_max);
        }
    }

    /// Hands the wait of a document that is still `in_progress` at the request deadline to a
    /// background task; the row stays `uploaded`.
    pub(super) fn spawn_background_indexing(
        &self,
        session: &UploadSession,
        attachment_id: Uuid,
        vector_store: String,
        provider_file: String,
    ) {
        let failure = AttachmentCleanupPayload {
            event_type: AttachmentCleanupEvent::IndexingFailed,
            tenant_id: session.ctx.subject_tenant_id(),
            chat_id: session.chat_id,
            attachment_id,
            provider_file_id: Some(provider_file.clone()),
            vector_store_id: None,
            storage_backend: session.storage.primary.backend.clone(),
            attachment_kind: AttachmentKind::Document.as_str().to_owned(),
            deleted_at: clock::now(),
            secondary_ref: None,
        };
        let task = BackgroundIndexing {
            ctx: session.ctx.clone(),
            scope: toolkit_security::AccessScope::for_tenant(session.ctx.subject_tenant_id()),
            route: session.storage.primary.clone(),
            repo: Arc::clone(&self.repo),
            storage: Arc::clone(&self.storage),
            metrics: Arc::clone(&self.metrics),
            timings: self.timings,
            shutdown: self.shutdown_token(),
            attachment_id,
            vector_store,
            provider_file,
            failure,
        };
        self.tasks.spawn(task.run());
    }
}
