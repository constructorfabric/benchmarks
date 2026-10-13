#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
//! Test doubles of the attachments domain: row builders, a recording metrics
//! sink, a scripted multipart source, a runtime, and an in-memory attachment
//! store with the same CAS semantics as the Secure ORM one (used by the
//! paused-time tests, where a real `SQLite` pool cannot run).

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::model::{
    AttachmentKind, AttachmentStatus, ChatLimits, CleanupRow, CleanupState, DeleteOutcome,
    FailOutcome, FailWithCleanup, FailureCode, NewAttachment, ReadyOutcome, SecondaryUpdate,
    SoftDelete, StaleUpload, StoreRow, StoredAttachment, Thumbnail,
};
use super::payload::{CleanupEvent, CleanupMessage, SecondaryRef};
use super::ports::{
    AttachmentMetrics, AttachmentRepository, BackgroundResult, BackgroundRuntime, UploadResult,
};
use super::source::{FilePartHeader, MultipartSource};
use crate::domain::error::{DomainError, MultipartIssue, ResourceKind};
use crate::domain::model::Chat;
use crate::domain::ports::ChatRepository;
use crate::test_doubles::{USER, USER_TENANT, s2s_ctx};

/// Base instant of the seeded rows (an hour before the clock of the fixtures).
pub fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_773_144_000).unwrap()
}

/// A stored row of the default user and tenant: a `ready` PDF document of chat
/// `chat(1)`; `edit` adjusts it.
pub fn stored(edit: impl FnOnce(&mut StoredAttachment)) -> StoredAttachment {
    let mut row = StoredAttachment {
        id: Uuid::from_u128(0xA7_0001),
        tenant_id: USER_TENANT,
        chat_id: Uuid::from_u128(0xC000_0001),
        uploaded_by_user_id: USER,
        filename: "report.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        size_bytes: 1234,
        storage_backend: "openai".to_owned(),
        provider_file_id: Some("file-1".to_owned()),
        status: AttachmentStatus::Ready,
        error_code: None,
        kind: AttachmentKind::Document,
        for_file_search: true,
        for_code_interpreter: false,
        thumbnail: None,
        cleanup_status: None,
        secondary_file_id: None,
        secondary_status: "not_attempted".to_owned(),
        created_at: t0(),
        updated_at: t0(),
        deleted_at: None,
    };
    edit(&mut row);
    row
}

// ---- metrics ------------------------------------------------------------------------------------------

/// One recorded instrument call.
#[derive(Debug, Clone, PartialEq)]
pub enum Metric {
    Upload(AttachmentKind, UploadResult),
    UploadBytes(AttachmentKind, u64),
    Pending(i64),
    Background(BackgroundResult),
    Abandoned(AttachmentStatus),
    ReaperScan,
    CleanupCompleted,
    CleanupFailed,
    CleanupRetry,
    SecondarySkipped,
}

/// Records every instrument call.
#[derive(Default)]
pub struct RecordingAttachmentMetrics {
    events: Mutex<Vec<Metric>>,
}

impl RecordingAttachmentMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<Metric> {
        self.events.lock().unwrap().clone()
    }

    /// The sum of the `pending_add` deltas.
    pub fn pending_balance(&self) -> i64 {
        self.events()
            .iter()
            .filter_map(|e| match e {
                Metric::Pending(d) => Some(*d),
                _ => None,
            })
            .sum()
    }

    pub fn uploads(&self) -> Vec<(AttachmentKind, UploadResult)> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Metric::Upload(k, r) => Some((k, r)),
                _ => None,
            })
            .collect()
    }

    pub fn background(&self) -> Vec<BackgroundResult> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                Metric::Background(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    pub fn count(&self, wanted: &Metric) -> usize {
        self.events().iter().filter(|e| *e == wanted).count()
    }

    fn push(&self, metric: Metric) {
        self.events.lock().unwrap().push(metric);
    }
}

impl AttachmentMetrics for RecordingAttachmentMetrics {
    fn upload(&self, kind: AttachmentKind, result: UploadResult) {
        self.push(Metric::Upload(kind, result));
    }
    fn upload_bytes(&self, kind: AttachmentKind, bytes: u64) {
        self.push(Metric::UploadBytes(kind, bytes));
    }
    fn pending_add(&self, delta: i64) {
        self.push(Metric::Pending(delta));
    }
    fn background_indexing(&self, result: BackgroundResult) {
        self.push(Metric::Background(result));
    }
    fn upload_abandoned(&self, from_status: AttachmentStatus) {
        self.push(Metric::Abandoned(from_status));
    }
    fn reaper_scan_seconds(&self, _seconds: f64) {
        self.push(Metric::ReaperScan);
    }
    fn cleanup_completed(&self) {
        self.push(Metric::CleanupCompleted);
    }
    fn cleanup_failed(&self) {
        self.push(Metric::CleanupFailed);
    }
    fn cleanup_retry(&self) {
        self.push(Metric::CleanupRetry);
    }
    fn secondary_cleanup_skipped(&self) {
        self.push(Metric::SecondarySkipped);
    }
}

// ---- multipart source -----------------------------------------------------------------------------------

/// A scripted multipart source: the header outcome, then chunks (or an error).
pub struct VecSource {
    header: Option<Result<FilePartHeader, MultipartIssue>>,
    chunks: VecDeque<Result<Bytes, MultipartIssue>>,
    opened: Arc<Mutex<bool>>,
    pulled: Arc<Mutex<usize>>,
}

/// Observers of a [`VecSource`] that outlive its borrow.
#[derive(Clone)]
pub struct SourceProbe {
    opened: Arc<Mutex<bool>>,
    pulled: Arc<Mutex<usize>>,
}

impl SourceProbe {
    /// `open_file` was called.
    pub fn opened(&self) -> bool {
        *self.opened.lock().unwrap()
    }

    /// Number of chunks pulled.
    pub fn pulled(&self) -> usize {
        *self.pulled.lock().unwrap()
    }
}

impl VecSource {
    /// A source of `header` and the given chunks.
    pub fn new(
        header: Result<FilePartHeader, MultipartIssue>,
        chunks: Vec<Result<Bytes, MultipartIssue>>,
    ) -> (Self, SourceProbe) {
        let opened = Arc::new(Mutex::new(false));
        let pulled = Arc::new(Mutex::new(0));
        (
            Self {
                header: Some(header),
                chunks: chunks.into(),
                opened: Arc::clone(&opened),
                pulled: Arc::clone(&pulled),
            },
            SourceProbe { opened, pulled },
        )
    }

    /// A well-formed source: `content` in chunks of `chunk` bytes.
    pub fn file(
        filename: Option<&str>,
        content_type: &str,
        content: &[u8],
        chunk: usize,
    ) -> (Self, SourceProbe) {
        let chunks = content
            .chunks(chunk.max(1))
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        Self::new(
            Ok(FilePartHeader {
                content_type: content_type.to_owned(),
                filename: filename.map(str::to_owned),
            }),
            chunks,
        )
    }
}

#[async_trait]
impl MultipartSource for VecSource {
    async fn open_file(&mut self) -> Result<FilePartHeader, MultipartIssue> {
        *self.opened.lock().unwrap() = true;
        self.header.take().expect("open_file is called once")
    }

    async fn next_chunk(&mut self) -> Result<Option<Bytes>, MultipartIssue> {
        *self.pulled.lock().unwrap() += 1;
        match self.chunks.pop_front() {
            Some(Ok(chunk)) => Ok(Some(chunk)),
            Some(Err(issue)) => Err(issue),
            None => Ok(None),
        }
    }
}

// ---- runtime --------------------------------------------------------------------------------------------

/// A runtime that spawns on tokio, records the task names and can be unbound.
pub struct TestRuntime {
    stop: CancellationToken,
    bound: bool,
    spawned: Mutex<Vec<&'static str>>,
    handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl TestRuntime {
    /// A bound runtime (the gear has started).
    pub fn bound() -> Arc<Self> {
        Arc::new(Self::build(true))
    }

    /// An unbound runtime (before `start`): nothing spawns, no S2S context.
    pub fn unbound() -> Arc<Self> {
        Arc::new(Self::build(false))
    }

    fn build(bound: bool) -> Self {
        Self {
            stop: CancellationToken::new(),
            bound,
            spawned: Mutex::new(Vec::new()),
            handles: Mutex::new(Vec::new()),
        }
    }

    /// Names of the spawned tasks.
    pub fn spawned(&self) -> Vec<&'static str> {
        self.spawned.lock().unwrap().clone()
    }

    /// Cancels the stop token (the gear stops).
    pub fn stop(&self) {
        self.stop.cancel();
    }

    /// Waits for every spawned task to end.
    pub async fn join(&self) {
        let handles: Vec<_> = std::mem::take(&mut *self.handles.lock().unwrap());
        for handle in handles {
            handle.await.unwrap();
        }
    }

    /// Number of spawned tasks that have not ended.
    pub fn running(&self) -> usize {
        self.handles
            .lock()
            .unwrap()
            .iter()
            .filter(|h| !h.is_finished())
            .count()
    }
}

impl BackgroundRuntime for TestRuntime {
    fn spawn(&self, name: &'static str, task: BoxFuture<'static, ()>) -> bool {
        if !self.bound {
            return false;
        }
        self.spawned.lock().unwrap().push(name);
        self.handles.lock().unwrap().push(tokio::spawn(task));
        true
    }

    fn stop_token(&self) -> CancellationToken {
        self.stop.clone()
    }

    fn s2s_context(&self) -> Option<SecurityContext> {
        self.bound.then(s2s_ctx)
    }
}

// ---- chats ----------------------------------------------------------------------------------------------

/// A chat store with one kind of answer: `load` returns the seeded chats.
#[derive(Default)]
pub struct FakeChats {
    chats: Mutex<Vec<Chat>>,
}

impl FakeChats {
    pub fn with(chats: Vec<Chat>) -> Arc<Self> {
        Arc::new(Self {
            chats: Mutex::new(chats),
        })
    }

    /// A chat of the default user and tenant on `model`.
    pub fn chat(id: Uuid, model: &str) -> Chat {
        Chat {
            id,
            tenant_id: USER_TENANT,
            user_id: USER,
            model: model.to_owned(),
            title: None,
            is_temporary: false,
            created_at: t0(),
            updated_at: t0(),
        }
    }
}

#[async_trait]
impl ChatRepository for FakeChats {
    async fn insert(&self, _scope: &AccessScope, _chat: &Chat) -> Result<(), DomainError> {
        unimplemented!("not used")
    }

    async fn load(&self, _scope: &AccessScope, chat_id: Uuid) -> Result<Chat, DomainError> {
        self.chats
            .lock()
            .unwrap()
            .iter()
            .find(|c| c.id == chat_id)
            .cloned()
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })
    }

    async fn message_counts(
        &self,
        _scope: &AccessScope,
        _chat_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, u64>, DomainError> {
        unimplemented!("not used")
    }

    async fn rename(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
        _title: &str,
        _at: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        unimplemented!("not used")
    }

    async fn page(
        &self,
        _scope: &AccessScope,
        _query: &toolkit_odata::ODataQuery,
    ) -> Result<toolkit_odata::Page<Chat>, DomainError> {
        unimplemented!("not used")
    }

    async fn soft_delete_with_cleanup(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<(), DomainError> {
        unimplemented!("not used")
    }
}

// ---- the in-memory attachment store ------------------------------------------------------------------

/// Everything the in-memory store holds.
#[derive(Default)]
pub struct FakeState {
    pub rows: BTreeMap<Uuid, StoredAttachment>,
    pub stores: Vec<StoreRow>,
    /// Cleanup messages in enqueue order.
    pub messages: Vec<CleanupMessage>,
    /// Soft-deleted chats.
    pub deleted_chats: HashSet<Uuid>,
    /// Attachments referenced by a `message_attachments` row.
    pub linked: HashSet<Uuid>,
    /// `(id, now)` of every successful heartbeat.
    pub heartbeats: Vec<(Uuid, OffsetDateTime)>,
    /// Method names in call order.
    pub calls: Vec<&'static str>,
    /// `cleanup_attempts` per row.
    pub attempts: BTreeMap<Uuid, i32>,
}

/// In-memory [`AttachmentRepository`]: the CAS guards of the real store, a call
/// log and injectable database failures.
#[derive(Default)]
pub struct FakeRepo {
    pub state: Mutex<FakeState>,
    failures: Mutex<Vec<(&'static str, usize)>>,
}

impl FakeRepo {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The next `times` calls of `method` fail with a database error.
    pub fn fail(&self, method: &'static str, times: usize) {
        self.failures.lock().unwrap().push((method, times));
    }

    pub fn put(&self, row: StoredAttachment) {
        self.state.lock().unwrap().rows.insert(row.id, row);
    }

    pub fn row(&self, id: Uuid) -> StoredAttachment {
        self.state.lock().unwrap().rows.get(&id).cloned().unwrap()
    }

    /// The only row (tests of one upload).
    pub fn only_row(&self) -> StoredAttachment {
        let state = self.state.lock().unwrap();
        assert_eq!(state.rows.len(), 1, "exactly one row");
        state.rows.values().next().cloned().unwrap()
    }

    pub fn rows(&self) -> Vec<StoredAttachment> {
        self.state.lock().unwrap().rows.values().cloned().collect()
    }

    pub fn messages(&self) -> Vec<CleanupMessage> {
        self.state.lock().unwrap().messages.clone()
    }

    pub fn stores(&self) -> Vec<StoreRow> {
        self.state.lock().unwrap().stores.clone()
    }

    pub fn put_store(&self, row: StoreRow) {
        self.state.lock().unwrap().stores.push(row);
    }

    pub fn calls(&self, method: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| **c == method)
            .count()
    }

    pub fn delete_chat(&self, chat_id: Uuid) {
        self.state.lock().unwrap().deleted_chats.insert(chat_id);
    }

    pub fn link(&self, attachment_id: Uuid) {
        self.state.lock().unwrap().linked.insert(attachment_id);
    }

    /// Applies `edit` to the row under the lock.
    pub fn edit(&self, id: Uuid, edit: impl FnOnce(&mut StoredAttachment)) {
        let mut state = self.state.lock().unwrap();
        edit(state.rows.get_mut(&id).unwrap());
    }

    fn enter(&self, method: &'static str) -> Result<(), DomainError> {
        self.state.lock().unwrap().calls.push(method);
        let mut failures = self.failures.lock().unwrap();
        if let Some(at) = failures
            .iter()
            .position(|(name, times)| *name == method && *times > 0)
        {
            failures[at].1 -= 1;
            return Err(DomainError::Database(format!(
                "injected failure of {method}"
            )));
        }
        Ok(())
    }

    fn enqueue(state: &mut FakeState, message: &CleanupMessage) {
        state.messages.push(message.clone());
    }
}

fn live(row: &StoredAttachment) -> bool {
    row.deleted_at.is_none() && row.cleanup_status.is_none()
}

#[async_trait]
impl AttachmentRepository for FakeRepo {
    async fn find(
        &self,
        _scope: &AccessScope,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<StoredAttachment>, DomainError> {
        self.enter("find")?;
        Ok(self
            .state
            .lock()
            .unwrap()
            .rows
            .get(&attachment_id)
            .filter(|r| r.chat_id == chat_id)
            .cloned())
    }

    async fn vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoreRow>, DomainError> {
        self.enter("vector_store")?;
        Ok(self
            .state
            .lock()
            .unwrap()
            .stores
            .iter()
            .find(|s| s.tenant_id == tenant_id && s.chat_id == chat_id)
            .cloned())
    }

    async fn insert_pending(
        &self,
        new: &NewAttachment,
        limits: &ChatLimits,
    ) -> Result<StoredAttachment, DomainError> {
        self.enter("insert_pending")?;
        let mut state = self.state.lock().unwrap();
        if state.deleted_chats.contains(&new.chat_id) {
            return Err(DomainError::NotFound {
                resource: ResourceKind::Chat,
            });
        }
        let counted = |r: &&StoredAttachment| {
            r.chat_id == new.chat_id
                && r.deleted_at.is_none()
                && r.status != AttachmentStatus::Failed
        };
        let docs = state
            .rows
            .values()
            .filter(counted)
            .filter(|r| r.kind == AttachmentKind::Document)
            .count()
            + usize::from(new.kind == AttachmentKind::Document);
        if new.kind == AttachmentKind::Document && docs > limits.max_documents as usize {
            return Err(DomainError::DocumentLimit);
        }
        let total: i64 = state
            .rows
            .values()
            .filter(counted)
            .map(|r| r.size_bytes)
            .sum::<i64>()
            + new.size_bytes;
        if u64::try_from(total).unwrap_or(u64::MAX) > limits.max_total_bytes {
            return Err(DomainError::StorageLimit);
        }
        let row = StoredAttachment {
            id: new.id,
            tenant_id: new.tenant_id,
            chat_id: new.chat_id,
            uploaded_by_user_id: new.user_id,
            filename: new.filename.clone(),
            content_type: new.content_type.clone(),
            size_bytes: new.size_bytes,
            storage_backend: new.storage_backend.clone(),
            provider_file_id: None,
            status: AttachmentStatus::Pending,
            error_code: None,
            kind: new.kind,
            for_file_search: new.for_file_search,
            for_code_interpreter: new.for_code_interpreter,
            thumbnail: None,
            cleanup_status: None,
            secondary_file_id: None,
            secondary_status: "not_attempted".to_owned(),
            created_at: new.now,
            updated_at: new.now,
            deleted_at: None,
        };
        state.rows.insert(row.id, row.clone());
        Ok(row)
    }

    async fn set_uploaded(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.enter("set_uploaded")?;
        let mut state = self.state.lock().unwrap();
        let Some(row) = state.rows.get_mut(&id) else {
            return Ok(false);
        };
        if row.tenant_id != tenant_id || row.status != AttachmentStatus::Pending || !live(row) {
            return Ok(false);
        }
        row.status = AttachmentStatus::Uploaded;
        row.provider_file_id = Some(provider_file_id.to_owned());
        row.updated_at = now;
        Ok(true)
    }

    async fn set_ready(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        thumbnail: Option<&Thumbnail>,
        now: OffsetDateTime,
    ) -> Result<ReadyOutcome, DomainError> {
        self.enter("set_ready")?;
        let mut state = self.state.lock().unwrap();
        let chat_deleted;
        {
            let Some(row) = state.rows.get_mut(&id) else {
                return Ok(ReadyOutcome::Taken {
                    chat_deleted: false,
                });
            };
            if row.tenant_id == tenant_id
                && row.status == AttachmentStatus::Uploaded
                && row.provider_file_id.is_some()
                && live(row)
            {
                row.status = AttachmentStatus::Ready;
                row.updated_at = now;
                if let Some(thumbnail) = thumbnail {
                    row.thumbnail = Some(thumbnail.clone());
                }
                return Ok(ReadyOutcome::Ready);
            }
            chat_deleted = row.chat_id;
        }
        Ok(ReadyOutcome::Taken {
            chat_deleted: state.deleted_chats.contains(&chat_deleted),
        })
    }

    async fn set_failed(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        code: FailureCode,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.enter("set_failed")?;
        let mut state = self.state.lock().unwrap();
        let Some(row) = state.rows.get_mut(&id) else {
            return Ok(false);
        };
        if row.tenant_id != tenant_id
            || !matches!(
                row.status,
                AttachmentStatus::Pending | AttachmentStatus::Uploaded
            )
            || row.deleted_at.is_some()
        {
            return Ok(false);
        }
        row.status = AttachmentStatus::Failed;
        row.error_code = Some(code.as_str().to_owned());
        row.updated_at = now;
        Ok(true)
    }

    async fn fail_with_cleanup(
        &self,
        request: &FailWithCleanup,
    ) -> Result<FailOutcome, DomainError> {
        self.enter("fail_with_cleanup")?;
        let mut state = self.state.lock().unwrap();
        let won = state
            .rows
            .get_mut(&request.attachment_id)
            .is_some_and(|row| {
                if row.tenant_id == request.tenant_id
                    && matches!(
                        row.status,
                        AttachmentStatus::Pending | AttachmentStatus::Uploaded
                    )
                    && live(row)
                {
                    row.status = AttachmentStatus::Failed;
                    row.error_code = Some(request.code.as_str().to_owned());
                    row.provider_file_id = Some(request.provider_file_id.clone());
                    row.cleanup_status = Some("pending".to_owned());
                    row.updated_at = request.now;
                    true
                } else {
                    false
                }
            });
        if won {
            Self::enqueue(&mut state, &request.message);
            return Ok(FailOutcome::Failed);
        }
        if let Some(orphan) = &request.orphan {
            Self::enqueue(&mut state, orphan);
            return Ok(FailOutcome::OrphanEnqueued);
        }
        Ok(FailOutcome::RowTaken)
    }

    async fn enqueue_orphan_file(&self, message: &CleanupMessage) -> Result<(), DomainError> {
        self.enter("enqueue_orphan_file")?;
        Self::enqueue(&mut self.state.lock().unwrap(), message);
        Ok(())
    }

    async fn set_secondary(
        &self,
        _tenant_id: Uuid,
        id: Uuid,
        update: &SecondaryUpdate,
        _now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        self.enter("set_secondary")?;
        let mut state = self.state.lock().unwrap();
        if let Some(row) = state.rows.get_mut(&id) {
            match update {
                SecondaryUpdate::Pending => "pending".clone_into(&mut row.secondary_status),
                SecondaryUpdate::Uploaded(file) => {
                    "uploaded".clone_into(&mut row.secondary_status);
                    row.secondary_file_id = Some(file.clone());
                }
                SecondaryUpdate::Failed => "failed".clone_into(&mut row.secondary_status),
            }
        }
        Ok(())
    }

    async fn insert_store_placeholder(&self, row: &StoreRow) -> Result<(), DomainError> {
        self.enter("insert_store_placeholder")?;
        let mut state = self.state.lock().unwrap();
        if state.deleted_chats.contains(&row.chat_id) {
            return Err(DomainError::NotFound {
                resource: ResourceKind::Chat,
            });
        }
        if state
            .stores
            .iter()
            .any(|s| s.tenant_id == row.tenant_id && s.chat_id == row.chat_id)
        {
            return Err(DomainError::UniqueViolation);
        }
        state.stores.push(row.clone());
        Ok(())
    }

    async fn bind_store(
        &self,
        tenant_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        self.enter("bind_store")?;
        let mut state = self.state.lock().unwrap();
        let Some(row) = state
            .stores
            .iter_mut()
            .find(|s| s.id == row_id && s.tenant_id == tenant_id && s.vector_store_id.is_none())
        else {
            return Ok(false);
        };
        row.vector_store_id = Some(vector_store_id.to_owned());
        Ok(true)
    }

    async fn delete_placeholder(&self, tenant_id: Uuid, row_id: Uuid) -> Result<bool, DomainError> {
        self.enter("delete_placeholder")?;
        let mut state = self.state.lock().unwrap();
        let before = state.stores.len();
        state.stores.retain(|s| {
            !(s.id == row_id && s.tenant_id == tenant_id && s.vector_store_id.is_none())
        });
        Ok(state.stores.len() < before)
    }

    async fn heartbeat(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.enter("heartbeat")?;
        let mut state = self.state.lock().unwrap();
        let Some(row) = state.rows.get_mut(&id) else {
            return Ok(false);
        };
        if row.tenant_id != tenant_id || row.status != AttachmentStatus::Uploaded || !live(row) {
            return Ok(false);
        }
        row.updated_at = now;
        state.heartbeats.push((id, now));
        Ok(true)
    }

    async fn soft_delete(
        &self,
        _scope: &AccessScope,
        request: &SoftDelete,
    ) -> Result<DeleteOutcome, DomainError> {
        self.enter("soft_delete")?;
        let mut state = self.state.lock().unwrap();
        let linked = state.linked.contains(&request.attachment_id);
        let Some(row) = state.rows.get_mut(&request.attachment_id) else {
            return Err(DomainError::NotFound {
                resource: ResourceKind::Attachment,
            });
        };
        if row.deleted_at.is_some() {
            return Ok(DeleteOutcome::AlreadyDeleted);
        }
        if linked {
            return Ok(DeleteOutcome::Locked);
        }
        let had_cleanup = row.cleanup_status.is_some();
        row.deleted_at = Some(request.now);
        row.updated_at = request.now;
        if !had_cleanup {
            row.cleanup_status = Some("pending".to_owned());
            let secondary = request
                .secondary_alias
                .clone()
                .zip(row.secondary_file_id.clone())
                .filter(|_| row.secondary_status == "uploaded")
                .map(|(upstream_alias, file_id)| SecondaryRef {
                    file_id,
                    upstream_alias,
                });
            let message = CleanupMessage::for_row(
                CleanupEvent::AttachmentDeleted,
                row,
                row.provider_file_id.clone(),
                secondary,
                request.now,
            );
            Self::enqueue(&mut state, &message);
        }
        Ok(DeleteOutcome::Deleted)
    }

    async fn stale_uploads(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<StaleUpload>, DomainError> {
        self.enter("stale_uploads")?;
        let state = self.state.lock().unwrap();
        let mut rows: Vec<&StoredAttachment> = state
            .rows
            .values()
            .filter(|r| {
                matches!(
                    r.status,
                    AttachmentStatus::Pending | AttachmentStatus::Uploaded
                ) && live(r)
                    && r.updated_at < cutoff
            })
            .collect();
        rows.sort_by_key(|r| r.updated_at);
        Ok(rows
            .into_iter()
            .take(limit as usize)
            .map(|r| StaleUpload {
                id: r.id,
                tenant_id: r.tenant_id,
                chat_id: r.chat_id,
                status: r.status,
                provider_file_id: r.provider_file_id.clone(),
                secondary_file_id: r.secondary_file_id.clone(),
                storage_backend: r.storage_backend.clone(),
                kind: r.kind,
                updated_at: r.updated_at,
            })
            .collect())
    }

    async fn abandon(
        &self,
        stale: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.enter("abandon")?;
        let mut guard = self.state.lock().unwrap();
        let Some(row) = guard.rows.get_mut(&stale.id) else {
            return Ok(false);
        };
        if row.status != stale.status || !live(row) || row.updated_at >= cutoff {
            return Ok(false);
        }
        row.status = AttachmentStatus::Failed;
        row.error_code = Some("upload_abandoned".to_owned());
        row.updated_at = now;
        if row.provider_file_id.is_some() {
            row.cleanup_status = Some("pending".to_owned());
            let message = CleanupMessage::for_row(
                CleanupEvent::AttachmentUploadAbandoned,
                row,
                row.provider_file_id.clone(),
                None,
                now,
            );
            Self::enqueue(&mut guard, &message);
        }
        Ok(true)
    }

    async fn chat_deleted(
        &self,
        _tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<bool>, DomainError> {
        self.enter("chat_deleted")?;
        Ok(Some(
            self.state.lock().unwrap().deleted_chats.contains(&chat_id),
        ))
    }

    async fn cleanup_row(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<CleanupRow>, DomainError> {
        self.enter("cleanup_row")?;
        let state = self.state.lock().unwrap();
        Ok(state.rows.get(&id).map(|r| CleanupRow {
            state: match r.cleanup_status.as_deref() {
                Some("pending") => Some(CleanupState::Pending),
                Some("done") => Some(CleanupState::Done),
                Some("failed") => Some(CleanupState::Failed),
                _ => None,
            },
            attempts: state.attempts.get(&id).copied().unwrap_or(0),
        }))
    }

    async fn record_cleanup_attempt(
        &self,
        _tenant_id: Uuid,
        id: Uuid,
        _error: &str,
        _now: OffsetDateTime,
    ) -> Result<Option<i32>, DomainError> {
        self.enter("record_cleanup_attempt")?;
        let mut state = self.state.lock().unwrap();
        let pending = state
            .rows
            .get(&id)
            .is_some_and(|r| r.cleanup_status.as_deref() == Some("pending"));
        if !pending {
            return Ok(None);
        }
        let attempts = state.attempts.entry(id).or_insert(0);
        *attempts += 1;
        Ok(Some(*attempts))
    }

    async fn finish_cleanup(
        &self,
        _tenant_id: Uuid,
        id: Uuid,
        state_to: CleanupState,
        _now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.enter("finish_cleanup")?;
        let mut state = self.state.lock().unwrap();
        let Some(row) = state.rows.get_mut(&id) else {
            return Ok(false);
        };
        if row.cleanup_status.as_deref() != Some("pending") {
            return Ok(false);
        }
        row.cleanup_status = Some(state_to.as_str().to_owned());
        Ok(true)
    }
}
