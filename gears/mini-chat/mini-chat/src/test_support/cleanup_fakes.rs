//! Fakes of the cleanup handler of FEATURE 0005 slice S2: an in-memory [`CleanupStore`] with the
//! guarded-update semantics of the port and per-method error injection, a delete-only
//! [`StorageProvider`] that records the target of each call, a fault wrapper for the real
//! database store, and a controllable chat read.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachments::cleanup::{AttemptRecord, CleanupFrom, CleanupRow, CleanupStore};
use crate::domain::attachments::provider::{
    FileUploadMeta, ProviderFailure, StorageProvider, StorageTarget, UploadBody, UploadError,
};
use crate::domain::error::DomainError;

// ── the in-memory store ──────────────────────────────────────────────────────

/// One call the fake store received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupCall {
    ChatRead,
    RowRead,
    FailedAttempt { cause: String, max_attempts: u32 },
    MarkDone(CleanupFrom),
    ReplayFailure { cause: String },
}

/// The state of the one attachment row the fake serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRow {
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i32,
    pub last_cleanup_error: Option<String>,
    pub cleanup_updated_at: Option<OffsetDateTime>,
}

/// An in-memory [`CleanupStore`] for one chat and one attachment.
pub struct FakeCleanupStore {
    chat: Mutex<Option<bool>>,
    row: Mutex<Option<FakeRow>>,
    calls: Mutex<Vec<CleanupCall>>,
    /// Methods that answer an internal error instead of running.
    pub fail_chat_read: AtomicBool,
    pub fail_row_read: AtomicBool,
    pub fail_attempt: AtomicBool,
    pub fail_done: AtomicBool,
    pub fail_replay_failure: AtomicBool,
}

impl FakeCleanupStore {
    /// A live chat and a row in `cleanup_status` (`None` is NULL) with `attempts` attempts.
    pub fn with_row(cleanup_status: Option<&str>, attempts: i32) -> Arc<Self> {
        Arc::new(Self {
            chat: Mutex::new(Some(true)),
            row: Mutex::new(Some(FakeRow {
                cleanup_status: cleanup_status.map(str::to_owned),
                cleanup_attempts: attempts,
                last_cleanup_error: None,
                cleanup_updated_at: None,
            })),
            calls: Mutex::new(Vec::new()),
            fail_chat_read: AtomicBool::new(false),
            fail_row_read: AtomicBool::new(false),
            fail_attempt: AtomicBool::new(false),
            fail_done: AtomicBool::new(false),
            fail_replay_failure: AtomicBool::new(false),
        })
    }

    /// The chat is soft-deleted.
    pub fn chat_deleted(self: &Arc<Self>) -> Arc<Self> {
        *self.chat.lock().unwrap() = Some(false);
        Arc::clone(self)
    }

    /// The chat does not exist.
    pub fn chat_missing(self: &Arc<Self>) -> Arc<Self> {
        *self.chat.lock().unwrap() = None;
        Arc::clone(self)
    }

    /// The row does not exist.
    pub fn row_missing(self: &Arc<Self>) -> Arc<Self> {
        *self.row.lock().unwrap() = None;
        Arc::clone(self)
    }

    pub fn row(&self) -> Option<FakeRow> {
        self.row.lock().unwrap().clone()
    }

    pub fn calls(&self) -> Vec<CleanupCall> {
        self.calls.lock().unwrap().clone()
    }

    /// The writes the store saw (everything but the two reads).
    pub fn writes(&self) -> Vec<CleanupCall> {
        self.calls()
            .into_iter()
            .filter(|call| !matches!(call, CleanupCall::ChatRead | CleanupCall::RowRead))
            .collect()
    }

    fn injected(what: &str) -> DomainError {
        DomainError::internal(format!("injected {what} failure"))
    }
}

#[async_trait]
impl CleanupStore for FakeCleanupStore {
    async fn chat_is_live(&self, _tenant_id: Uuid, _chat_id: Uuid) -> Result<bool, DomainError> {
        self.calls.lock().unwrap().push(CleanupCall::ChatRead);
        if self.fail_chat_read.load(Ordering::SeqCst) {
            return Err(Self::injected("chat read"));
        }
        Ok(*self.chat.lock().unwrap() == Some(true))
    }

    async fn load_row(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
        _attachment_id: Uuid,
    ) -> Result<Option<CleanupRow>, DomainError> {
        self.calls.lock().unwrap().push(CleanupCall::RowRead);
        if self.fail_row_read.load(Ordering::SeqCst) {
            return Err(Self::injected("row read"));
        }
        Ok(self.row.lock().unwrap().as_ref().map(|row| CleanupRow {
            cleanup_status: row.cleanup_status.clone(),
            cleanup_attempts: row.cleanup_attempts,
        }))
    }

    async fn record_failed_attempt(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        cause: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<Option<AttemptRecord>, DomainError> {
        self.calls.lock().unwrap().push(CleanupCall::FailedAttempt {
            cause: cause.to_owned(),
            max_attempts,
        });
        if self.fail_attempt.load(Ordering::SeqCst) {
            return Err(Self::injected("attempt write"));
        }
        let mut guard = self.row.lock().unwrap();
        let Some(row) = guard.as_mut() else {
            return Ok(None);
        };
        if row.cleanup_status.as_deref() != Some("pending") {
            return Ok(None);
        }
        row.cleanup_attempts += 1;
        row.last_cleanup_error = Some(cause.to_owned());
        row.cleanup_updated_at = Some(now);
        let exhausted = u32::try_from(row.cleanup_attempts).unwrap_or(0) >= max_attempts;
        if exhausted {
            row.cleanup_status = Some("failed".to_owned());
        }
        Ok(Some(AttemptRecord {
            attempts: row.cleanup_attempts,
            exhausted,
        }))
    }

    async fn mark_done(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        from: CleanupFrom,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.calls.lock().unwrap().push(CleanupCall::MarkDone(from));
        if self.fail_done.load(Ordering::SeqCst) {
            return Err(Self::injected("done write"));
        }
        let mut guard = self.row.lock().unwrap();
        let Some(row) = guard.as_mut() else {
            return Ok(false);
        };
        let expected = match from {
            CleanupFrom::Pending => "pending",
            CleanupFrom::Failed => "failed",
        };
        if row.cleanup_status.as_deref() != Some(expected) {
            return Ok(false);
        }
        row.cleanup_status = Some("done".to_owned());
        row.cleanup_updated_at = Some(now);
        Ok(true)
    }

    async fn record_replay_failure(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        cause: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.calls.lock().unwrap().push(CleanupCall::ReplayFailure {
            cause: cause.to_owned(),
        });
        if self.fail_replay_failure.load(Ordering::SeqCst) {
            return Err(Self::injected("replay-failure write"));
        }
        let mut guard = self.row.lock().unwrap();
        let Some(row) = guard.as_mut() else {
            return Ok(false);
        };
        if row.cleanup_status.as_deref() != Some("failed") {
            return Ok(false);
        }
        row.last_cleanup_error = Some(cause.to_owned());
        row.cleanup_updated_at = Some(now);
        Ok(true)
    }
}

// ── the real store with faults ───────────────────────────────────────────────

/// The armed faults of a [`FaultyCleanupStore`]: an armed method answers an internal error
/// instead of running (nothing is written for it).
#[derive(Default)]
pub struct CleanupFaults {
    pub chat_read: AtomicBool,
    pub row_read: AtomicBool,
    pub attempt: AtomicBool,
    pub done: AtomicBool,
    pub replay_failure: AtomicBool,
}

/// A [`CleanupStore`] with fault injection in front of another one (the database store).
pub struct FaultyCleanupStore {
    pub inner: Arc<dyn CleanupStore>,
    pub faults: Arc<CleanupFaults>,
}

impl FaultyCleanupStore {
    fn injected(what: &str) -> DomainError {
        DomainError::internal(format!("injected {what} failure"))
    }
}

#[async_trait]
impl CleanupStore for FaultyCleanupStore {
    async fn chat_is_live(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError> {
        if self.faults.chat_read.load(Ordering::SeqCst) {
            return Err(Self::injected("chat read"));
        }
        self.inner.chat_is_live(tenant_id, chat_id).await
    }

    async fn load_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<CleanupRow>, DomainError> {
        if self.faults.row_read.load(Ordering::SeqCst) {
            return Err(Self::injected("row read"));
        }
        self.inner.load_row(tenant_id, chat_id, attachment_id).await
    }

    async fn record_failed_attempt(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<Option<AttemptRecord>, DomainError> {
        if self.faults.attempt.load(Ordering::SeqCst) {
            return Err(Self::injected("attempt write"));
        }
        self.inner
            .record_failed_attempt(tenant_id, attachment_id, cause, max_attempts, now)
            .await
    }

    async fn mark_done(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        from: CleanupFrom,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        if self.faults.done.load(Ordering::SeqCst) {
            return Err(Self::injected("done write"));
        }
        self.inner
            .mark_done(tenant_id, attachment_id, from, now)
            .await
    }

    async fn record_replay_failure(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        if self.faults.replay_failure.load(Ordering::SeqCst) {
            return Err(Self::injected("replay-failure write"));
        }
        self.inner
            .record_replay_failure(tenant_id, attachment_id, cause, now)
            .await
    }
}

// ── a delete-only provider ───────────────────────────────────────────────────

/// One delete the provider received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteCall {
    /// `true` for `delete_secondary_file`.
    pub secondary: bool,
    pub upstream_alias: String,
    pub storage_backend: String,
    pub file_id: String,
}

/// A [`StorageProvider`] that serves the file deletes of the cleanup handler (queued answers,
/// success by default, a call log with the target) and refuses every other call: the handler
/// makes no vector store call.
#[derive(Default)]
pub struct DeletingProvider {
    primary: Mutex<VecDeque<Result<(), ProviderFailure>>>,
    secondary: Mutex<VecDeque<Result<(), ProviderFailure>>>,
    calls: Mutex<Vec<DeleteCall>>,
    other_calls: Mutex<usize>,
}

impl DeletingProvider {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The answer of the next primary file delete.
    pub fn queue_primary(&self, answer: Result<(), ProviderFailure>) {
        self.primary.lock().unwrap().push_back(answer);
    }

    /// The answer of the next secondary file delete.
    pub fn queue_secondary(&self, answer: Result<(), ProviderFailure>) {
        self.secondary.lock().unwrap().push_back(answer);
    }

    pub fn calls(&self) -> Vec<DeleteCall> {
        self.calls.lock().unwrap().clone()
    }

    /// The primary file deletes.
    pub fn primary_calls(&self) -> Vec<DeleteCall> {
        self.calls()
            .into_iter()
            .filter(|call| !call.secondary)
            .collect()
    }

    /// The secondary file deletes.
    pub fn secondary_calls(&self) -> Vec<DeleteCall> {
        self.calls()
            .into_iter()
            .filter(|call| call.secondary)
            .collect()
    }

    /// Calls of any method other than the two deletes (a vector store call, an upload).
    pub fn other_calls(&self) -> usize {
        *self.other_calls.lock().unwrap()
    }

    fn other<T>(&self, answer: T) -> T {
        *self.other_calls.lock().unwrap() += 1;
        answer
    }
}

#[async_trait]
impl StorageProvider for DeletingProvider {
    async fn upload_file(
        &self,
        _target: &StorageTarget,
        _meta: &FileUploadMeta,
        _body: UploadBody,
    ) -> Result<String, UploadError> {
        self.other(Err(UploadError::Provider))
    }

    async fn delete_file(
        &self,
        target: &StorageTarget,
        provider_file_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.calls.lock().unwrap().push(DeleteCall {
            secondary: false,
            upstream_alias: target.upstream_alias.clone(),
            storage_backend: target.storage_backend.clone(),
            file_id: provider_file_id.to_owned(),
        });
        self.primary.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }

    async fn create_vector_store(
        &self,
        _target: &StorageTarget,
    ) -> Result<String, ProviderFailure> {
        self.other(Err(ProviderFailure::Other))
    }

    async fn delete_vector_store(
        &self,
        _target: &StorageTarget,
        _vector_store_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.other(Err(ProviderFailure::Other))
    }

    async fn add_vector_store_file(
        &self,
        _target: &StorageTarget,
        _vector_store_id: &str,
        _provider_file_id: &str,
        _attachment_id: Uuid,
    ) -> Result<Option<String>, ProviderFailure> {
        self.other(Err(ProviderFailure::Other))
    }

    async fn read_vector_store_file(
        &self,
        _target: &StorageTarget,
        _vector_store_id: &str,
        _provider_file_id: &str,
        _timeout: Duration,
    ) -> Result<Option<String>, ProviderFailure> {
        self.other(Err(ProviderFailure::Other))
    }

    async fn upload_secondary_image(
        &self,
        _target: &StorageTarget,
        _filename: &str,
        _content_type: &str,
        _bytes: Bytes,
    ) -> Result<String, ProviderFailure> {
        self.other(Err(ProviderFailure::Other))
    }

    async fn delete_secondary_file(
        &self,
        target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.calls.lock().unwrap().push(DeleteCall {
            secondary: true,
            upstream_alias: target.upstream_alias.clone(),
            storage_backend: target.storage_backend.clone(),
            file_id: file_id.to_owned(),
        });
        self.secondary.lock().unwrap().pop_front().unwrap_or(Ok(()))
    }
}
