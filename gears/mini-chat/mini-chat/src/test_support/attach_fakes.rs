//! Fakes of the attachment ports for the domain unit tests: a recording metrics double, a scripted
//! storage provider, a scripted attachment store (the background indexing writes) and an
//! in-memory `chat_vector_stores` repository with the real unique, guarded-delete and
//! compare-and-set semantics. Every double stamps its calls with the virtual time of the
//! [`ManualTime`] it shares with the code under test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::too_many_lines,
    reason = "test support"
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use uuid::Uuid;

use super::manual_time::ManualTime;
use super::provider::Gate;
use crate::config::StorageKind;
use crate::domain::attachments::AttachmentMetrics;
use crate::domain::attachments::limits::ChatLimits;
use crate::domain::attachments::model::{AttachmentRecord, AttachmentStatus, Thumbnail};
use crate::domain::attachments::provider::{
    FileUploadMeta, ProviderFailure, StorageProvider, StorageTarget, UploadBody, UploadError,
};
use crate::domain::attachments::store::{
    AttachmentCleanupMessage, AttachmentCleanupQueue, AttachmentStore, DeleteOutcome,
    DeleteRequest, NewAttachment, SecondaryUpdate,
};
use crate::domain::attachments::vector_store::{
    ChatVectorStoreRepo, PlaceholderInsert, VectorStoreRow,
};
use crate::domain::error::DomainError;

// ── metrics ──────────────────────────────────────────────────────────────────

/// One recorded attachment metric sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricEvent {
    Upload { kind: String, result: String },
    UploadBytes { kind: String, bytes: u64 },
    Pending(i64),
    Background(String),
    Abandoned(String),
    ReaperScan(Duration),
    CleanupCompleted,
    CleanupRetry(String),
    CleanupFailed,
    SecondarySkipped(String),
    Knowledge(String),
    KnowledgeLatency(Duration),
    KnowledgeChunks(usize),
}

/// A metrics double that keeps every sample in order.
#[derive(Default)]
pub struct RecordingAttachmentMetrics {
    events: Mutex<Vec<MetricEvent>>,
}

impl RecordingAttachmentMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<MetricEvent> {
        self.events.lock().unwrap().clone()
    }

    /// The `(kind, result)` of every `attachment_upload` sample.
    pub fn uploads(&self) -> Vec<(String, String)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::Upload { kind, result } => Some((kind, result)),
                _ => None,
            })
            .collect()
    }

    /// The `(kind, bytes)` of every `attachment_upload_bytes` sample.
    pub fn upload_bytes(&self) -> Vec<(String, u64)> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::UploadBytes { kind, bytes } => Some((kind, bytes)),
                _ => None,
            })
            .collect()
    }

    /// The sum of the `attachments_pending` deltas.
    pub fn pending_total(&self) -> i64 {
        self.events()
            .into_iter()
            .map(|event| match event {
                MetricEvent::Pending(delta) => delta,
                _ => 0,
            })
            .sum()
    }

    /// The `result` of every `attachment_background_indexing` sample.
    pub fn background(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::Background(result) => Some(result),
                _ => None,
            })
            .collect()
    }

    /// The `from_status` of every `attachment_upload_abandoned` sample.
    pub fn abandoned(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::Abandoned(status) => Some(status),
                _ => None,
            })
            .collect()
    }

    /// The durations of the `upload_reaper_scan_duration_seconds` samples.
    pub fn reaper_scans(&self) -> Vec<Duration> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::ReaperScan(elapsed) => Some(elapsed),
                _ => None,
            })
            .collect()
    }

    /// How many `cleanup_completed` samples were recorded.
    pub fn cleanup_completed(&self) -> usize {
        self.count_of(|event| matches!(event, MetricEvent::CleanupCompleted))
    }

    /// The `reason` of every `cleanup_retry` sample.
    pub fn cleanup_retries(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::CleanupRetry(reason) => Some(reason),
                _ => None,
            })
            .collect()
    }

    /// How many `cleanup_failed` samples were recorded.
    pub fn cleanup_failed(&self) -> usize {
        self.count_of(|event| matches!(event, MetricEvent::CleanupFailed))
    }

    /// The `provider_kind` of every `secondary_cleanup_skipped` sample.
    pub fn secondary_skipped(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::SecondarySkipped(kind) => Some(kind),
                _ => None,
            })
            .collect()
    }

    /// The `result` of every `knowledge_search` sample.
    pub fn knowledge(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::Knowledge(result) => Some(result),
                _ => None,
            })
            .collect()
    }

    /// The number of `knowledge_search_latency_ms` samples.
    pub fn knowledge_latencies(&self) -> usize {
        self.count_of(|event| matches!(event, MetricEvent::KnowledgeLatency(_)))
    }

    /// The chunk counts of the `knowledge_search_chunks` samples.
    pub fn knowledge_chunks(&self) -> Vec<usize> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                MetricEvent::KnowledgeChunks(chunks) => Some(chunks),
                _ => None,
            })
            .collect()
    }

    fn count_of(&self, pick: impl Fn(&MetricEvent) -> bool) -> usize {
        self.events().iter().filter(|event| pick(event)).count()
    }

    fn push(&self, event: MetricEvent) {
        self.events.lock().unwrap().push(event);
    }
}

impl AttachmentMetrics for RecordingAttachmentMetrics {
    fn upload(&self, kind: &str, result: &str) {
        self.events.lock().unwrap().push(MetricEvent::Upload {
            kind: kind.to_owned(),
            result: result.to_owned(),
        });
    }

    fn upload_bytes(&self, kind: &str, bytes: u64) {
        self.events.lock().unwrap().push(MetricEvent::UploadBytes {
            kind: kind.to_owned(),
            bytes,
        });
    }

    fn pending_add(&self, delta: i64) {
        self.events
            .lock()
            .unwrap()
            .push(MetricEvent::Pending(delta));
    }

    fn background_indexing(&self, result: &str) {
        self.events
            .lock()
            .unwrap()
            .push(MetricEvent::Background(result.to_owned()));
    }

    fn upload_abandoned(&self, from_status: &str) {
        self.push(MetricEvent::Abandoned(from_status.to_owned()));
    }

    fn reaper_scan_duration(&self, elapsed: Duration) {
        self.push(MetricEvent::ReaperScan(elapsed));
    }

    fn cleanup_completed(&self) {
        self.push(MetricEvent::CleanupCompleted);
    }

    fn cleanup_retry(&self, reason: &str) {
        self.push(MetricEvent::CleanupRetry(reason.to_owned()));
    }

    fn cleanup_failed(&self) {
        self.push(MetricEvent::CleanupFailed);
    }

    fn secondary_cleanup_skipped(&self, provider_kind: &str) {
        self.push(MetricEvent::SecondarySkipped(provider_kind.to_owned()));
    }

    fn knowledge_search(&self, result: &str) {
        self.push(MetricEvent::Knowledge(result.to_owned()));
    }

    fn knowledge_search_latency(&self, elapsed: Duration) {
        self.push(MetricEvent::KnowledgeLatency(elapsed));
    }

    fn knowledge_search_chunks(&self, chunks: usize) {
        self.push(MetricEvent::KnowledgeChunks(chunks));
    }
}

// ── storage provider ─────────────────────────────────────────────────────────

/// A storage target of the OpenAI kind for the unit tests.
pub fn openai_target() -> StorageTarget {
    StorageTarget {
        provider_id: "azure_openai".to_owned(),
        upstream_alias: "azure.example.com".to_owned(),
        storage_kind: StorageKind::Openai,
        api_version: None,
        storage_backend: "azure_openai".to_owned(),
    }
}

/// One call the scripted provider received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCall {
    Upload {
        filename: String,
        content_type: String,
        bytes: usize,
    },
    DeleteFile(String),
    CreateVectorStore,
    DeleteVectorStore(String),
    Add {
        vector_store_id: String,
        file_id: String,
        attachment_id: Uuid,
    },
    Read {
        vector_store_id: String,
        file_id: String,
        timeout: Duration,
    },
    UploadSecondary {
        filename: String,
        content_type: String,
        bytes: usize,
    },
    DeleteSecondary(String),
}

type Status = Result<Option<String>, ProviderFailure>;

/// A scripted [`StorageProvider`]: queued answers per call, defaults when a queue is empty, a
/// gate in front of the vector store create, and a call log stamped with the virtual time.
pub struct ScriptedProvider {
    time: Arc<ManualTime>,
    calls: Mutex<Vec<(Duration, ProviderCall)>>,
    add: Mutex<VecDeque<Status>>,
    reads: Mutex<VecDeque<Status>>,
    default_read: Mutex<Status>,
    creates: Mutex<VecDeque<Result<String, ProviderFailure>>>,
    create_gate: Mutex<Option<Arc<Gate>>>,
    uploads: Mutex<VecDeque<Result<String, UploadError>>>,
    secondaries: Mutex<VecDeque<Result<String, ProviderFailure>>>,
    read_gate: Mutex<Option<Arc<Gate>>>,
}

impl ScriptedProvider {
    pub fn new(time: &Arc<ManualTime>) -> Arc<Self> {
        Arc::new(Self {
            time: Arc::clone(time),
            calls: Mutex::new(Vec::new()),
            add: Mutex::new(VecDeque::new()),
            reads: Mutex::new(VecDeque::new()),
            default_read: Mutex::new(Ok(Some("in_progress".to_owned()))),
            creates: Mutex::new(VecDeque::new()),
            create_gate: Mutex::new(None),
            uploads: Mutex::new(VecDeque::new()),
            secondaries: Mutex::new(VecDeque::new()),
            read_gate: Mutex::new(None),
        })
    }

    /// The answer of the next `add_vector_store_file` (default: `in_progress`).
    pub fn queue_add(&self, status: Status) {
        self.add.lock().unwrap().push_back(status);
    }

    /// The answer of the next `read_vector_store_file`.
    pub fn queue_read(&self, status: Status) {
        self.reads.lock().unwrap().push_back(status);
    }

    /// The next read answers the status text.
    pub fn queue_read_status(&self, text: &str) {
        self.queue_read(Ok(Some(text.to_owned())));
    }

    /// The next add answers the status text.
    pub fn queue_add_status(&self, text: &str) {
        self.queue_add(Ok(Some(text.to_owned())));
    }

    /// The answer of a read when the queue is empty (default: `in_progress`).
    pub fn set_default_read(&self, status: Status) {
        *self.default_read.lock().unwrap() = status;
    }

    pub fn queue_create(&self, answer: Result<String, ProviderFailure>) {
        self.creates.lock().unwrap().push_back(answer);
    }

    /// Park every `create_vector_store` on `gate` until it is opened.
    pub fn gate_create(&self, gate: &Arc<Gate>) {
        *self.create_gate.lock().unwrap() = Some(Arc::clone(gate));
    }

    /// Park every `read_vector_store_file` on `gate` until it is opened.
    pub fn gate_reads(&self, gate: &Arc<Gate>) {
        *self.read_gate.lock().unwrap() = Some(Arc::clone(gate));
    }

    /// Every call with the virtual time it arrived at.
    pub fn calls(&self) -> Vec<(Duration, ProviderCall)> {
        self.calls.lock().unwrap().clone()
    }

    /// The virtual times of the calls selected by `pick`.
    pub fn times_of(&self, pick: impl Fn(&ProviderCall) -> bool) -> Vec<Duration> {
        self.calls()
            .into_iter()
            .filter(|(_, call)| pick(call))
            .map(|(at, _)| at)
            .collect()
    }

    pub fn count_where(&self, pick: impl Fn(&ProviderCall) -> bool) -> usize {
        self.times_of(pick).len()
    }

    pub fn reads(&self) -> Vec<(Duration, Duration)> {
        self.calls()
            .into_iter()
            .filter_map(|(at, call)| match call {
                ProviderCall::Read { timeout, .. } => Some((at, timeout)),
                _ => None,
            })
            .collect()
    }

    fn record(&self, call: ProviderCall) {
        let at = self.time.elapsed();
        self.calls.lock().unwrap().push((at, call));
    }
}

#[async_trait]
impl StorageProvider for ScriptedProvider {
    async fn upload_file(
        &self,
        _target: &StorageTarget,
        meta: &FileUploadMeta,
        mut body: UploadBody,
    ) -> Result<String, UploadError> {
        let mut total = 0usize;
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => total += bytes.len(),
                Err(error) => return Err(UploadError::Body(error)),
            }
        }
        self.record(ProviderCall::Upload {
            filename: meta.provider_filename.clone(),
            content_type: meta.content_type.clone(),
            bytes: total,
        });
        self.uploads
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok("file-1".to_owned()))
    }

    async fn delete_file(
        &self,
        _target: &StorageTarget,
        provider_file_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.record(ProviderCall::DeleteFile(provider_file_id.to_owned()));
        Ok(())
    }

    async fn create_vector_store(
        &self,
        _target: &StorageTarget,
    ) -> Result<String, ProviderFailure> {
        self.record(ProviderCall::CreateVectorStore);
        let gate = self.create_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait().await;
        }
        self.creates
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok("vs_created".to_owned()))
    }

    async fn delete_vector_store(
        &self,
        _target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.record(ProviderCall::DeleteVectorStore(vector_store_id.to_owned()));
        Ok(())
    }

    async fn add_vector_store_file(
        &self,
        _target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
        attachment_id: Uuid,
    ) -> Result<Option<String>, ProviderFailure> {
        self.record(ProviderCall::Add {
            vector_store_id: vector_store_id.to_owned(),
            file_id: provider_file_id.to_owned(),
            attachment_id,
        });
        self.add
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(Some("in_progress".to_owned())))
    }

    async fn read_vector_store_file(
        &self,
        _target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, ProviderFailure> {
        self.record(ProviderCall::Read {
            vector_store_id: vector_store_id.to_owned(),
            file_id: provider_file_id.to_owned(),
            timeout,
        });
        let gate = self.read_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.wait().await;
        }
        let queued = self.reads.lock().unwrap().pop_front();
        queued.unwrap_or_else(|| self.default_read.lock().unwrap().clone())
    }

    async fn upload_secondary_image(
        &self,
        _target: &StorageTarget,
        filename: &str,
        content_type: &str,
        bytes: Bytes,
    ) -> Result<String, ProviderFailure> {
        self.record(ProviderCall::UploadSecondary {
            filename: filename.to_owned(),
            content_type: content_type.to_owned(),
            bytes: bytes.len(),
        });
        self.secondaries
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok("file_ant_1".to_owned()))
    }

    async fn delete_secondary_file(
        &self,
        _target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.record(ProviderCall::DeleteSecondary(file_id.to_owned()));
        Ok(())
    }
}

// ── attachment store (background indexing writes) ────────────────────────────

/// One write of the background task as the scripted store saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreCall {
    Heartbeat { at: Duration, now: OffsetDateTime },
    SetReady { at: Duration },
    FailIndexing { at: Duration, chat_id: Uuid },
}

/// A scripted [`AttachmentStore`] that serves the background indexing writes only; every other
/// method answers an internal error.
pub struct ScriptedStore {
    time: Arc<ManualTime>,
    calls: Mutex<Vec<StoreCall>>,
    heartbeats: Mutex<VecDeque<Result<bool, DomainError>>>,
    ready: Mutex<VecDeque<Result<bool, DomainError>>>,
    fail: Mutex<VecDeque<Result<bool, DomainError>>>,
}

impl ScriptedStore {
    pub fn new(time: &Arc<ManualTime>) -> Arc<Self> {
        Arc::new(Self {
            time: Arc::clone(time),
            calls: Mutex::new(Vec::new()),
            heartbeats: Mutex::new(VecDeque::new()),
            ready: Mutex::new(VecDeque::new()),
            fail: Mutex::new(VecDeque::new()),
        })
    }

    /// The answer of the next heartbeat (default: a row was refreshed).
    pub fn queue_heartbeat(&self, answer: Result<bool, DomainError>) {
        self.heartbeats.lock().unwrap().push_back(answer);
    }

    /// The answer of the next guarded `ready` write (default: a row was updated).
    pub fn queue_ready(&self, answer: Result<bool, DomainError>) {
        self.ready.lock().unwrap().push_back(answer);
    }

    /// The answer of the next failure transaction (default: committed).
    pub fn queue_fail(&self, answer: Result<bool, DomainError>) {
        self.fail.lock().unwrap().push_back(answer);
    }

    pub fn calls(&self) -> Vec<StoreCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn heartbeat_times(&self) -> Vec<Duration> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                StoreCall::Heartbeat { at, .. } => Some(at),
                _ => None,
            })
            .collect()
    }

    pub fn ready_times(&self) -> Vec<Duration> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                StoreCall::SetReady { at } => Some(at),
                _ => None,
            })
            .collect()
    }

    pub fn fail_count(&self) -> usize {
        self.calls()
            .iter()
            .filter(|call| matches!(call, StoreCall::FailIndexing { .. }))
            .count()
    }

    fn unsupported() -> DomainError {
        DomainError::internal("the scripted store serves only the background writes")
    }
}

#[async_trait]
impl AttachmentStore for ScriptedStore {
    async fn insert_pending(
        &self,
        _new: &NewAttachment,
        _limits: &ChatLimits,
    ) -> Result<AttachmentRecord, DomainError> {
        Err(Self::unsupported())
    }

    async fn mark_uploaded(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        _provider_file_id: &str,
        _size_bytes: i64,
        _now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        Err(Self::unsupported())
    }

    async fn storage_recheck(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
        _attachment_id: Uuid,
        _final_bytes: i64,
        _limits: &ChatLimits,
    ) -> Result<bool, DomainError> {
        Err(Self::unsupported())
    }

    async fn mark_failed(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        _error_code: &str,
        _now: OffsetDateTime,
    ) -> Result<Option<AttachmentStatus>, DomainError> {
        Err(Self::unsupported())
    }

    async fn set_secondary(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        _update: &SecondaryUpdate,
        _now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        Err(Self::unsupported())
    }

    async fn mark_ready(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        _thumbnail: Option<&Thumbnail>,
        _now: OffsetDateTime,
    ) -> Result<(), DomainError> {
        Err(Self::unsupported())
    }

    async fn get(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
        _attachment_id: Uuid,
        _uploader_id: Uuid,
    ) -> Result<AttachmentRecord, DomainError> {
        Err(Self::unsupported())
    }

    async fn heartbeat(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.calls.lock().unwrap().push(StoreCall::Heartbeat {
            at: self.time.elapsed(),
            now,
        });
        self.heartbeats
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(true))
    }

    async fn set_ready_guarded(
        &self,
        _tenant_id: Uuid,
        _attachment_id: Uuid,
        _now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.calls.lock().unwrap().push(StoreCall::SetReady {
            at: self.time.elapsed(),
        });
        self.ready.lock().unwrap().pop_front().unwrap_or(Ok(true))
    }

    async fn fail_indexing(
        &self,
        _tenant_id: Uuid,
        chat_id: Uuid,
        _attachment_id: Uuid,
        _now: OffsetDateTime,
        _cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<bool, DomainError> {
        self.calls.lock().unwrap().push(StoreCall::FailIndexing {
            at: self.time.elapsed(),
            chat_id,
        });
        self.fail.lock().unwrap().pop_front().unwrap_or(Ok(true))
    }

    async fn delete_attachment(
        &self,
        _request: &DeleteRequest,
        _cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<DeleteOutcome, DomainError> {
        Err(Self::unsupported())
    }
}

/// A cleanup queue that must never be reached by the unit tests (the scripted store does not
/// enqueue).
pub struct UnusedCleanupQueue;

#[async_trait]
impl AttachmentCleanupQueue for UnusedCleanupQueue {
    async fn enqueue_attachment_cleanup(
        &self,
        _tx: &DbTx<'_>,
        _message: &AttachmentCleanupMessage,
    ) -> Result<Wake, DomainError> {
        Err(DomainError::internal("the unit tests never enqueue"))
    }
}

// ── chat_vector_stores repository ────────────────────────────────────────────

/// One repository call, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoCall {
    Find { at: Duration },
    InsertPlaceholder { provider: String },
    DeletePlaceholder,
    SetVectorStoreId { vector_store_id: String },
}

/// An in-memory [`ChatVectorStoreRepo`] with `UNIQUE(tenant_id, chat_id)`, the guarded
/// placeholder delete and the compare-and-set.
/// What the competing request left behind when it won the insert race.
#[derive(Debug, Clone)]
enum LostRace {
    Placeholder,
    Bound(String),
}

pub struct MemoryVectorRepo {
    time: Arc<ManualTime>,
    rows: Mutex<Vec<VectorStoreRow>>,
    calls: Mutex<Vec<RepoCall>>,
    lose_next: Mutex<Option<LostRace>>,
}

impl MemoryVectorRepo {
    pub fn new(time: &Arc<ManualTime>) -> Arc<Self> {
        Arc::new(Self {
            time: Arc::clone(time),
            rows: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            lose_next: Mutex::new(None),
        })
    }

    /// The next placeholder insert loses the race: another request's row (a placeholder, or
    /// bound to `vector_store_id`) appears first and the insert answers `AlreadyExists`.
    pub fn lose_next_insert(&self, vector_store_id: Option<&str>) {
        *self.lose_next.lock().unwrap() = Some(match vector_store_id {
            Some(id) => LostRace::Bound(id.to_owned()),
            None => LostRace::Placeholder,
        });
    }

    pub fn seed(&self, row: VectorStoreRow) {
        self.rows.lock().unwrap().push(row);
    }

    pub fn rows(&self) -> Vec<VectorStoreRow> {
        self.rows.lock().unwrap().clone()
    }

    /// Remove every row (another request reclaimed the placeholder).
    pub fn clear(&self) {
        self.rows.lock().unwrap().clear();
    }

    /// Bind the placeholder of the chat to `vector_store_id` the way a winning request would.
    pub fn bind(&self, vector_store_id: &str) {
        for row in self.rows.lock().unwrap().iter_mut() {
            row.vector_store_id = Some(vector_store_id.to_owned());
        }
    }

    pub fn calls(&self) -> Vec<RepoCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self, pick: impl Fn(&RepoCall) -> bool) -> usize {
        self.calls().iter().filter(|call| pick(call)).count()
    }
}

#[async_trait]
impl ChatVectorStoreRepo for MemoryVectorRepo {
    async fn find(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError> {
        self.calls.lock().unwrap().push(RepoCall::Find {
            at: self.time.elapsed(),
        });
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.tenant_id == tenant_id && row.chat_id == chat_id)
            .cloned())
    }

    async fn insert_placeholder(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        provider: &str,
        now: OffsetDateTime,
    ) -> Result<PlaceholderInsert, DomainError> {
        self.calls
            .lock()
            .unwrap()
            .push(RepoCall::InsertPlaceholder {
                provider: provider.to_owned(),
            });
        let mut rows = self.rows.lock().unwrap();
        if let Some(winner) = self.lose_next.lock().unwrap().take() {
            rows.push(VectorStoreRow {
                id: Uuid::new_v4(),
                tenant_id,
                chat_id,
                vector_store_id: match winner {
                    LostRace::Bound(id) => Some(id),
                    LostRace::Placeholder => None,
                },
                provider: provider.to_owned(),
                file_count: 0,
                created_at: now,
            });
            return Ok(PlaceholderInsert::AlreadyExists);
        }
        if rows
            .iter()
            .any(|row| row.tenant_id == tenant_id && row.chat_id == chat_id)
        {
            return Ok(PlaceholderInsert::AlreadyExists);
        }
        let id = Uuid::new_v4();
        rows.push(VectorStoreRow {
            id,
            tenant_id,
            chat_id,
            vector_store_id: None,
            provider: provider.to_owned(),
            file_count: 0,
            created_at: now,
        });
        Ok(PlaceholderInsert::Inserted(id))
    }

    async fn delete_placeholder(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
    ) -> Result<bool, DomainError> {
        self.calls.lock().unwrap().push(RepoCall::DeletePlaceholder);
        let mut rows = self.rows.lock().unwrap();
        let before = rows.len();
        rows.retain(|row| {
            !(row.id == row_id
                && row.tenant_id == tenant_id
                && row.chat_id == chat_id
                && row.vector_store_id.is_none())
        });
        Ok(rows.len() < before)
    }

    async fn set_vector_store_id(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError> {
        self.calls.lock().unwrap().push(RepoCall::SetVectorStoreId {
            vector_store_id: vector_store_id.to_owned(),
        });
        let mut rows = self.rows.lock().unwrap();
        for row in rows.iter_mut() {
            if row.id == row_id
                && row.tenant_id == tenant_id
                && row.chat_id == chat_id
                && row.vector_store_id.is_none()
            {
                row.vector_store_id = Some(vector_store_id.to_owned());
                return Ok(true);
            }
        }
        Ok(false)
    }
}
